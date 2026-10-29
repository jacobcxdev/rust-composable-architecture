//! Generates paths that borrow fields from the original state.
//!
//! Aggregate cases also embed owned fields. Shared helpers preserve source
//! generics and avoid caller-name collisions for the other derives.

use std::collections::HashMap;

use proc_macro2::TokenStream;
use quote::quote;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, Data, DeriveInput, Field, Fields, Generics, Ident, Lifetime, Type, Variant};

use crate::{canonical_ident, path_is_ident, rca_path, snake_case_ident};

/// Selects the field adapter, or omits a field or case constructor.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Projection {
    /// Borrows the whole field, including wrappers not explicitly projected.
    Required,
    /// Borrows a present child through `OptionalStatePayload`.
    Optional,
    /// Borrows keyed storage through `KeyedStatePayload`.
    Keyed,
    /// Keeps the source member but generates no constructor for it.
    Skip,
}

impl Projection {
    /// Returns the canonical spelling used in diagnostics, including raw inputs.
    const fn name(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
            Self::Keyed => "keyed",
            Self::Skip => "skip",
        }
    }
}

/// Lowers a complete derive input after rustc has applied conditional attributes.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    Ok(expand_parts(input, None)?.into_tokens())
}

/// Keeps owned top-level item boundaries available to conditional domains.
pub(crate) struct PathExpansion {
    /// Declares the constructor namespace before its implementation.
    pub(crate) namespace: TokenStream,
    /// Preserves marker and implementation boundaries for conditional callers.
    pub(crate) cases: Vec<CaseItems>,
    /// Implements the selected constructors on the namespace.
    pub(crate) constructors: TokenStream,
}

/// A case marker followed by its implementation block.
pub(crate) type CaseItems = [TokenStream; 2];

/// Keeps the full domain's mismatch arm when lowering one selected variant.
pub(crate) struct VariantContext<'a> {
    /// Supplies domain visibility, generics and names that generated bindings avoid.
    pub(crate) input: &'a DeriveInput,
    /// Supplies the selected case's fields and conditional attributes.
    pub(crate) variant: &'a Variant,
    /// Matches other enabled cases, even when the caller lowers only this variant.
    pub(crate) fallback: &'a TokenStream,
}

impl PathExpansion {
    /// Moves each emitted item into source order without an intermediate buffer.
    pub(crate) fn into_tokens(self) -> TokenStream {
        let Self {
            mut namespace,
            cases,
            constructors,
        } = self;
        namespace.extend(
            cases
                .into_iter()
                .flatten()
                .chain(std::iter::once(constructors)),
        );
        namespace
    }
}

/// Keeps conditional item boundaries and accepts a full-domain mismatch arm.
///
/// Direct derives infer that arm from their enabled variants. Reducer domains
/// supply it when they lower one variant under a symbolic condition.
pub(crate) fn expand_parts(
    input: &DeriveInput,
    enum_fallback: Option<&TokenStream>,
) -> syn::Result<PathExpansion> {
    validate_type_attributes(&input.attrs)?;
    let rca = rca_path()?;
    let state = &input.ident;
    let paths = paths_ident(state)?;
    let visibility = &input.vis;
    let generics = &input.generics;
    let projection_lifetime = projection_lifetime(input);
    let state_generics = type_arguments(generics);
    let state_type: Type = syn::parse_quote!(#state #state_generics);
    let rewritten_generics = source_generics(generics, &state_type);

    let (constructors, cases) = match &input.data {
        Data::Struct(data) => (
            expand_struct(&data.fields, &state_type, &projection_lifetime, &rca, input)?,
            Vec::new(),
        ),
        Data::Enum(data) => {
            let fallback = enum_fallback.cloned().unwrap_or_else(|| {
                if data.variants.len() > 1 {
                    quote!(_ => ::core::option::Option::None,)
                } else {
                    TokenStream::new()
                }
            });
            expand_enum(
                &data.variants,
                &state_type,
                &projection_lifetime,
                &rca,
                input,
                &fallback,
            )?
        }
        Data::Union(data) => {
            return Err(syn::Error::new_spanned(
                data.union_token,
                "StatePaths can only be derived for a named struct or enum",
            ));
        }
    };
    let (impl_generics, _, where_clause) = rewritten_generics.split_for_impl();
    let type_generics = type_arguments(&rewritten_generics);
    let (declaration_generics, declaration_where_clause) =
        declaration_generics(generics, &state_type);
    let namespace = if generics.type_params().next().is_some() {
        generic_namespace(input, &paths, &state_type)
    } else if generics.params.is_empty() {
        quote!(
            /// Constructs borrowed field and enum-case paths for the source state.
            #[derive(::core::fmt::Debug)]
            #visibility struct #paths #declaration_generics #declaration_where_clause;
        )
    } else {
        quote!(
            /// Constructs borrowed field and enum-case paths for the source state.
            #[derive(::core::fmt::Debug)]
            #visibility struct #paths #declaration_generics(
                ::core::marker::PhantomData<fn() -> #state_type>
            ) #declaration_where_clause;
        )
    };

    Ok(PathExpansion {
        namespace: quote!(#[automatically_derived] #namespace),
        cases,
        constructors: quote! {
            #[automatically_derived]
            impl #impl_generics #paths #type_generics #where_clause {
                #(#constructors)*
            }
        },
    })
}

/// Gives generic constructors a namespace without imposing payload trait bounds.
///
/// The alias retains source visibility and bounds. Its hidden, uninhabited storage
/// avoids deriving `Debug` from payload types or exposing a constructible value.
pub(crate) fn generic_namespace(input: &DeriveInput, paths: &Ident, source: &Type) -> TokenStream {
    let module = fresh_binding(
        input,
        &format!("__rca_path_namespace_{}", canonical_ident(paths)),
    );
    let visibility = &input.vis;
    let (generics, where_clause) = declaration_generics(&input.generics, source);
    quote! {
        #[doc(hidden)]
        #[allow(non_snake_case, reason = "generated namespace storage retains domain spelling")]
        mod #module {
            // Keep generated names isolated from caller constants.
            /// Stores the source domain type without constructing a value.
            #[allow(missing_debug_implementations, reason = "namespace has no values")]
            pub struct Namespace<Domain: ?Sized>(
                ::core::marker::PhantomData<fn() -> Domain>,
                ::core::convert::Infallible,
            );
        }
        /// Groups generated operations for the source domain.
        #[allow(type_alias_bounds, reason = "domain well-formedness and path impl retain original bounds")]
        #visibility type #paths #generics #where_clause = #module::Namespace<#source>;
    }
}

/// Emits named-field paths in declaration order, stopping at the first error.
fn expand_struct(
    fields: &Fields,
    state: &Type,
    lifetime: &Lifetime,
    rca: &TokenStream,
    input: &DeriveInput,
) -> syn::Result<Vec<TokenStream>> {
    let Fields::Named(fields) = fields else {
        return Err(syn::Error::new_spanned(
            fields,
            "StatePaths can only derive paths from named struct fields",
        ));
    };

    fields
        .named
        .iter()
        .map(|field| expand_field(field, state, lifetime, rca, input))
        .filter_map(Result::transpose)
        .collect()
}

/// Preserves field visibility and emits shared and mutable views of its storage.
fn expand_field(
    field: &Field,
    state: &Type,
    lifetime: &Lifetime,
    rca: &TokenStream,
    input: &DeriveInput,
) -> syn::Result<Option<TokenStream>> {
    /// Contains only adapters that survive the early skip decision.
    enum FieldProjection {
        /// Borrows the field directly.
        Required,
        /// Borrows the optional wrapper's child.
        Optional,
        /// Borrows the wrapper's keyed state.
        Keyed,
    }

    let projection = match projection(&field.attrs)? {
        None | Some(Projection::Required) => FieldProjection::Required,
        Some(Projection::Optional) => FieldProjection::Optional,
        Some(Projection::Keyed) => FieldProjection::Keyed,
        Some(Projection::Skip) => return Ok(None),
    };

    let state_binding = fresh_binding(input, "state");
    let field_ident = field.ident.as_ref().expect("named fields are required");
    let field_type = source_type(&field.ty, state);
    let method = field_ident;
    let visibility = &field.vis;
    let constructor = match projection {
        FieldProjection::Required => quote! {
            /// Returns a path that borrows this field from the state.
            #visibility fn #method() -> #rca::StatePath<
                #state,
                #field_type,
                impl for<#lifetime> ::core::ops::Fn(&#lifetime #state) -> &#lifetime #field_type,
                impl for<#lifetime> ::core::ops::Fn(&#lifetime mut #state) -> &#lifetime mut #field_type,
            > {
                #[allow(unused_imports, reason = "alias shields generated binding from caller constants")]
                use ::core::mem::drop as #state_binding;
                #rca::StatePath::new(
                    |#state_binding: &#state| { &#state_binding.#field_ident },
                    |#state_binding: &mut #state| { &mut #state_binding.#field_ident },
                )
            }
        },
        FieldProjection::Optional => {
            let child = quote!(<#field_type as #rca::OptionalStatePayload>::Child);
            quote! {
                /// Returns a path that borrows this field's child when present.
                #visibility fn #method() -> #rca::OptionalStatePath<
                    #state,
                    #child,
                    impl for<#lifetime> ::core::ops::Fn(&#lifetime #state) -> ::core::option::Option<&#lifetime #child>,
                    impl for<#lifetime> ::core::ops::Fn(&#lifetime mut #state) -> ::core::option::Option<&#lifetime mut #child>,
                >
                where
                    #field_type: #rca::OptionalStatePayload,
                {
                    #[allow(unused_imports, reason = "alias shields generated binding from caller constants")]
                    use ::core::mem::drop as #state_binding;
                    #rca::OptionalStatePath::new(
                        |#state_binding: &#state| {
                            #rca::OptionalStatePayload::as_optional(&#state_binding.#field_ident)
                        },
                        |#state_binding: &mut #state| {
                            #rca::OptionalStatePayload::as_optional_mut(&mut #state_binding.#field_ident)
                        },
                    )
                }
            }
        }
        FieldProjection::Keyed => {
            let id = quote!(<#field_type as #rca::KeyedStatePayload>::Id);
            let child = quote!(<#field_type as #rca::KeyedStatePayload>::Child);
            quote! {
                /// Returns a path to this field's keyed child storage.
                #visibility fn #method() -> #rca::KeyedStatePath<
                    #state,
                    #id,
                    #child,
                    impl for<#lifetime> ::core::ops::Fn(&#lifetime #state) -> &#lifetime #rca::KeyedState<#id, #child>,
                    impl for<#lifetime> ::core::ops::Fn(&#lifetime mut #state) -> &#lifetime mut #rca::KeyedState<#id, #child>,
                >
                where
                    #field_type: #rca::KeyedStatePayload,
                {
                    #[allow(unused_imports, reason = "alias shields generated binding from caller constants")]
                    use ::core::mem::drop as #state_binding;
                    #rca::KeyedStatePath::new(
                        |#state_binding: &#state| {
                            #rca::KeyedStatePayload::as_keyed_state(&#state_binding.#field_ident)
                        },
                        |#state_binding: &mut #state| {
                            #rca::KeyedStatePayload::as_keyed_state_mut(&mut #state_binding.#field_ident)
                        },
                    )
                }
            }
        }
    };

    Ok(Some(constructor))
}

/// Emits case constructors in order and diagnoses canonical method-name collisions.
fn expand_enum(
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
    state: &Type,
    lifetime: &Lifetime,
    rca: &TokenStream,
    input: &DeriveInput,
    fallback: &TokenStream,
) -> syn::Result<(Vec<TokenStream>, Vec<CaseItems>)> {
    let mut constructors = Vec::new();
    let mut cases = Vec::new();
    let mut methods = HashMap::new();
    for variant in variants {
        let Some((method, constructor, case)) = expand_variant(
            &VariantContext {
                input,
                variant,
                fallback,
            },
            state,
            lifetime,
            rca,
        )?
        else {
            continue;
        };
        if let Some(previous) = methods.insert(canonical_ident(&method), &variant.ident) {
            let method_name = canonical_ident(&method);
            return Err(syn::Error::new_spanned(
                &variant.ident,
                format!(
                    "StatePaths variants `{previous}` and `{}` both produce path constructor `{method_name}`",
                    variant.ident
                ),
            ));
        }
        constructors.push(constructor);
        cases.extend(case);
    }
    Ok((constructors, cases))
}

/// Validates case helpers before selecting single-field or aggregate projection.
///
/// A named single field supplies its constructor name; other shapes use the case
/// name. Shared and mutable closures borrow the same matched storage.
fn expand_variant(
    context: &VariantContext<'_>,
    state: &Type,
    lifetime: &Lifetime,
    rca: &TokenStream,
) -> syn::Result<Option<(Ident, TokenStream, Option<CaseItems>)>> {
    let input = context.input;
    let variant = context.variant;
    let fallback = context.fallback;
    let state_ident = &input.ident;
    let visibility = &input.vis;
    let projection = projection(&variant.attrs)?;
    for field in &variant.fields {
        if let Some(attribute) = field
            .attrs
            .iter()
            .find(|attribute| path_is_ident(attribute.path(), "rca"))
        {
            return Err(syn::Error::new_spanned(
                attribute,
                "rca helper attributes on enum state belong on variants, not payload fields",
            ));
        }
    }
    if projection == Some(Projection::Skip) {
        return Ok(None);
    }
    if projection.is_some() {
        return Err(syn::Error::new_spanned(
            &variant.ident,
            "StatePaths enum variants only support #[rca(skip)]",
        ));
    }
    let state_binding = fresh_binding(input, "state");
    let child_binding = fresh_binding(input, "child");
    let variant_ident = &variant.ident;
    let mut fields = variant.fields.iter();
    let (method, child, pattern) = if let (Some(field), None) = (fields.next(), fields.next()) {
        let child = source_type(&field.ty, state);
        let (method, pattern) = if let Some(field_ident) = &field.ident {
            (
                field_ident.clone(),
                quote!(#state_ident::#variant_ident { #field_ident: #child_binding }),
            )
        } else {
            let method = snake_case_ident(variant_ident)?;
            (method, quote!(#state_ident::#variant_ident(#child_binding)))
        };
        (method, child, pattern)
    } else {
        let method = snake_case_ident(variant_ident)?;
        let (constructor, case) = aggregate_variant(context, state, &method, lifetime, rca, true);
        return Ok(Some((method, constructor, Some(case))));
    };

    let constructor = quote! {
        /// Returns a path that borrows the payload of this enum case.
        #visibility fn #method() -> #rca::CaseStatePath<
            #state,
            #child,
            impl for<#lifetime> ::core::ops::Fn(&#lifetime #state) -> ::core::option::Option<&#lifetime #child>,
            impl for<#lifetime> ::core::ops::Fn(&#lifetime mut #state) -> ::core::option::Option<&#lifetime mut #child>,
        > {
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{drop as #state_binding, drop as #child_binding};
            #rca::CaseStatePath::new(
                |#state_binding: &#state| {
                    match #state_binding {
                        #pattern => ::core::option::Option::Some(#child_binding),
                        #fallback
                    }
                },
                |#state_binding: &mut #state| {
                    match #state_binding {
                        #pattern => ::core::option::Option::Some(#child_binding),
                        #fallback
                    }
                },
            )
        }
    };
    Ok(Some((method, constructor, None)))
}

/// Generates tuple views for zero or multiple fields in declaration order.
///
/// State cases also expose disjoint mutable references. Both state and action
/// cases embed owned fields. Conditional attributes gate every generated item.
#[expect(
    clippy::too_many_lines,
    reason = "keeps the constructor, marker, and case implementations in one emitter"
)]
pub(crate) fn aggregate_variant(
    context: &VariantContext<'_>,
    root: &Type,
    method: &Ident,
    lifetime: &Lifetime,
    rca: &TokenStream,
    mutable: bool,
) -> (TokenStream, CaseItems) {
    let input = context.input;
    let variant = context.variant;
    let fallback = context.fallback;
    let root_ident = &input.ident;
    let variant_ident = &variant.ident;
    let visibility = &input.vis;
    let kind = if mutable { "state" } else { "action" };
    let marker = fresh_binding(
        input,
        &format!(
            "__rca_{kind}_case_{}_{}",
            canonical_ident(root_ident),
            canonical_ident(variant_ident)
        ),
    );
    let root_binding = fresh_binding(input, "__rca_root");
    let payload_binding = fresh_binding(input, "__rca_payload");
    let bindings: Vec<_> = variant
        .fields
        .iter()
        .enumerate()
        .map(|(index, _)| fresh_binding(input, &format!("__rca_field_{index}")))
        .collect();
    let types: Vec<_> = variant
        .fields
        .iter()
        .map(|field| source_type(&field.ty, root))
        .collect();
    let names: Vec<_> = variant
        .fields
        .iter()
        .filter_map(|field| field.ident.as_ref())
        .collect();
    let pattern = match &variant.fields {
        Fields::Unit => quote!(#root_ident::#variant_ident),
        Fields::Unnamed(_) => quote!(#root_ident::#variant_ident(#(#bindings),*)),
        Fields::Named(_) => quote!(#root_ident::#variant_ident { #(#names: #bindings),* }),
    };
    let attributes: Vec<_> = variant
        .attrs
        .iter()
        .filter(|attribute| {
            path_is_ident(attribute.path(), "cfg") || path_is_ident(attribute.path(), "cfg_attr")
        })
        .collect();
    let rewritten = source_generics(&input.generics, root);
    let (impl_generics, _, where_clause) = rewritten.split_for_impl();
    // The owned payload participates in the impl's effective reachability.
    // A public enum may validly contain a private payload (a warning, not E0446).
    let marker_type = quote!(#marker<#root, (#(#types,)*)>);
    let path = if mutable {
        quote!(#rca::AggregateStatePath)
    } else {
        quote!(#rca::AggregateActionPath)
    };
    let shield = quote! {
        #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
        use ::core::mem::{drop as #root_binding, drop as #payload_binding, #(drop as #bindings),*};
    };
    let mutable_impl = mutable.then(|| {
        quote! {
            #(#attributes)*
            #[automatically_derived]
            impl #impl_generics #rca::MutableAggregateCase for #marker_type #where_clause {
                type BorrowedMut<#lifetime> = (#(&#lifetime mut #types,)*)
                where
                    Self: #lifetime;

                fn get_mut<#lifetime>(
                    #root_binding: &#lifetime mut Self::Root
                ) -> ::core::option::Option<Self::BorrowedMut<#lifetime>>
                where
                    Self: #lifetime
                {
                    match #root_binding {
                        #pattern => ::core::option::Option::Some((#(#bindings,)*)),
                        #fallback
                    }
                }
            }
        }
    });
    let constructor = quote! {
        #(#attributes)*
        /// Projects fields in their declaration order, borrowing the original enum storage.
        #visibility fn #method() -> #path<#marker_type> {
            #path::new()
        }
    };
    let marker = quote! {
        #(#attributes)*
        #[doc(hidden)]
        #[allow(non_camel_case_types, reason = "generated case marker retains domain and variant spelling")]
        #visibility struct #marker<Root, Payload>(
            ::core::marker::PhantomData<fn() -> (Root, Payload)>
        );
    };
    let implementations = quote! {
        #(#attributes)*
        const _: () = {
            #shield
            #[automatically_derived]
            impl #impl_generics #rca::AggregateCase for #marker_type #where_clause {
                type Root = #root;
                type Payload = (#(#types,)*);
                type Borrowed<#lifetime> = (#(&#lifetime #types,)*)
                where
                    Self: #lifetime;

                fn extract<#lifetime>(
                    #root_binding: &#lifetime Self::Root
                ) -> ::core::option::Option<Self::Borrowed<#lifetime>>
                where
                    Self: #lifetime
                {
                    match #root_binding {
                        #pattern => ::core::option::Option::Some((#(#bindings,)*)),
                        #fallback
                    }
                }

                fn embed(#payload_binding: Self::Payload) -> Self::Root {
                    let (#(#bindings,)*) = #payload_binding;
                    #pattern
                }
            }
            #mutable_impl
        };
    };
    (constructor, [marker, implementations])
}

/// Parses one canonical option, rejecting values before duplicates or conflicts.
fn projection(attributes: &[Attribute]) -> syn::Result<Option<Projection>> {
    let mut projection = None;
    for attribute in attributes
        .iter()
        .filter(|attribute| path_is_ident(attribute.path(), "rca"))
    {
        let syn::Meta::List(list) = &attribute.meta else {
            return Err(syn::Error::new_spanned(
                attribute,
                "rca helper attributes require one option in parentheses",
            ));
        };
        if list.tokens.is_empty() {
            return Err(syn::Error::new_spanned(
                attribute,
                "rca helper attributes require one option",
            ));
        }
        attribute.parse_nested_meta(|meta| {
            let next = if path_is_ident(&meta.path, "required") {
                Projection::Required
            } else if path_is_ident(&meta.path, "optional") {
                Projection::Optional
            } else if path_is_ident(&meta.path, "keyed") {
                Projection::Keyed
            } else if path_is_ident(&meta.path, "skip") {
                Projection::Skip
            } else {
                return Err(meta.error(
                    "unknown rca option; expected `required`, `optional`, `keyed`, or `skip`",
                ));
            };

            if meta.input.peek(syn::Token![=]) || meta.input.peek(syn::token::Paren) {
                return Err(meta.error("rca options do not take values"));
            }

            if let Some(previous) = projection.replace(next) {
                let message = if previous == next {
                    format!("duplicate rca option `{}`", next.name())
                } else {
                    format!(
                        "conflicting rca options `{}` and `{}`",
                        previous.name(),
                        next.name()
                    )
                };
                return Err(meta.error(message));
            }
            Ok(())
        })?;
    }
    Ok(projection)
}

/// Rejects misplaced helpers before domain-shape and member validation.
fn validate_type_attributes(attributes: &[Attribute]) -> syn::Result<()> {
    if let Some(attribute) = attributes
        .iter()
        .find(|attribute| path_is_ident(attribute.path(), "rca"))
    {
        return Err(syn::Error::new_spanned(
            attribute,
            "rca helper attributes are only supported on struct fields or enum variants",
        ));
    }
    Ok(())
}

/// Separates rewritten parameters from bounds for the declaration's syntax.
pub(crate) fn declaration_generics(
    generics: &Generics,
    source: &Type,
) -> (Generics, Option<syn::WhereClause>) {
    let mut declaration = source_generics(generics, source);
    let where_clause = declaration.where_clause.take();
    (declaration, where_clause)
}

/// Emits arguments without defaults or bounds, bracing const arguments.
pub(crate) fn type_arguments(generics: &Generics) -> TokenStream {
    if generics.params.is_empty() {
        return TokenStream::new();
    }
    let arguments = generics.params.iter().map(|parameter| match parameter {
        syn::GenericParam::Lifetime(parameter) => {
            let lifetime = &parameter.lifetime;
            quote!(#lifetime)
        }
        syn::GenericParam::Type(parameter) => {
            let ident = &parameter.ident;
            quote!(#ident)
        }
        syn::GenericParam::Const(parameter) => {
            let ident = &parameter.ident;
            // Braces disambiguate constants from same-named namespace types.
            quote!({ #ident })
        }
    });
    quote!(< #(#arguments),* >)
}

/// Replaces source-domain `Self` before generics move onto generated items.
pub(crate) fn source_generics(generics: &Generics, source: &Type) -> Generics {
    let mut generics = generics.clone();
    SelfTypeRewriter { source }.visit_generics_mut(&mut generics);
    generics
}

/// Chooses a borrow lifetime absent from the full syntax, including nested binders.
///
/// Avoiding other identifier kinds too is conservative and lets this reuse the
/// binding-hygiene scan. Callers include any generated lifetimes already in scope.
pub(crate) fn projection_lifetime(input: &impl quote::ToTokens) -> Lifetime {
    let tokens = quote!(#input);
    let mut name = "__rca".to_owned();
    let mut suffix = 0_usize;
    // Finite source syntax contains finitely many identifier spellings, so a
    // fresh numeric suffix exists even when lifetimes are nested in field types.
    while contains_ident(tokens.clone(), &name) {
        suffix += 1;
        name = format!("__rca_{suffix}");
    }
    Lifetime::new(&format!("'{name}"), proc_macro2::Span::call_site())
}

/// Adds underscores until a valid internal base name avoids all source identifiers.
///
/// The scan includes nested token groups and raw spellings. Mixed-site hygiene
/// alone does not prevent collisions with caller constants or generic parameters.
pub(crate) fn fresh_binding(input: &impl quote::ToTokens, base: &str) -> Ident {
    let mut name = String::from(base);
    let tokens = quote!(#input);
    while contains_ident(tokens.clone(), &name) {
        name.push('_');
    }
    Ident::new(&name, proc_macro2::Span::mixed_site())
}

/// Looks through nested syntax without treating raw spelling as a different name.
fn contains_ident(tokens: TokenStream, name: &str) -> bool {
    tokens.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => {
            ident == name || ident.to_string().strip_prefix("r#") == Some(name)
        }
        proc_macro2::TokenTree::Group(group) => contains_ident(group.stream(), name),
        _ => false,
    })
}

/// Rewrites a cloned field type so generated-item `Self` cannot replace the domain.
pub(crate) fn source_type(field_type: &Type, state: &Type) -> Type {
    let mut field_type = field_type.clone();
    SelfTypeRewriter { source: state }.visit_type_mut(&mut field_type);
    field_type
}

/// Substitutes domain-relative type paths while preserving other type syntax.
struct SelfTypeRewriter<'a> {
    /// The original domain with its lifetime, type and const arguments applied.
    source: &'a Type,
}

impl VisitMut for SelfTypeRewriter<'_> {
    fn visit_type_path_mut(&mut self, i: &mut syn::TypePath) {
        if i.qself.is_none()
            && i.path
                .segments
                .first()
                .is_some_and(|segment| segment.ident == "Self")
        {
            let source = self.source;
            if i.path.segments.len() == 1 {
                *i = syn::parse_quote!(#source);
            } else {
                let segments = i.path.segments.iter().skip(1);
                *i = syn::parse_quote!(#source::#(#segments)::*);
            }
            return;
        }
        visit_mut::visit_type_path_mut(self, i);
    }
}

/// Validates the suffixed name, including malformed names rustc passes to derives.
fn paths_ident(state: &Ident) -> syn::Result<Ident> {
    let name = format!("{state}Paths");
    syn::parse_str(&name).map_err(|_parse_error| {
        syn::Error::new_spanned(
            state,
            "StatePaths cannot derive a valid path namespace name",
        )
    })
}

#[cfg(test)]
mod tests {
    use quote::{ToTokens, quote};
    use syn::DeriveInput;

    use super::expand;

    fn projection_match_arm_counts(tokens: proc_macro2::TokenStream) -> Vec<usize> {
        struct MatchArms(Vec<usize>);

        impl syn::visit_mut::VisitMut for MatchArms {
            fn visit_expr_match_mut(&mut self, i: &mut syn::ExprMatch) {
                self.0.push(i.arms.len());
                syn::visit_mut::visit_expr_match_mut(self, i);
            }
        }

        let mut output: syn::File = syn::parse2(tokens).unwrap();
        let mut matches = MatchArms(Vec::new());
        syn::visit_mut::VisitMut::visit_file_mut(&mut matches, &mut output);
        matches.0
    }

    // A sole case must not emit a mismatch branch no valid domain value can take.
    #[test]
    fn single_variant_paths_omit_mismatch_fallbacks() {
        for variant in [
            "Ready(u8)",
            "Ready { value: u8 }",
            "Ready",
            "Ready()",
            "Ready {}",
            "Ready(u8, u16)",
            "Ready { value: u8, revision: u16 }",
        ] {
            let input: DeriveInput =
                syn::parse_str(&format!("enum Domain {{ {variant} }}")).unwrap();
            assert_eq!(
                projection_match_arm_counts(expand(&input).unwrap()),
                [1, 1],
                "shared and mutable state projections: {variant}",
            );
            assert_eq!(
                projection_match_arm_counts(crate::action_paths::expand(&input).unwrap()),
                [1],
                "action extraction: {variant}",
            );
        }
        for variant in [
            "#[rca(keyed)] Ready(rca::KeyedAction<u8, u16>)",
            "#[rca(keyed)] Ready { value: rca::KeyedAction<u8, u16> }",
        ] {
            let input: DeriveInput =
                syn::parse_str(&format!("enum Domain {{ {variant} }}")).unwrap();
            assert_eq!(
                projection_match_arm_counts(crate::action_paths::expand(&input).unwrap()),
                [1],
                "keyed action extraction: {variant}",
            );
        }
    }

    // Skipping a path constructor does not remove its variant from the enum.
    #[test]
    fn skipped_variant_paths_retain_mismatch_fallbacks() {
        for variant in [
            "Ready(u8)",
            "Ready { value: u8 }",
            "Ready",
            "Ready()",
            "Ready {}",
            "Ready(u8, u16)",
            "Ready { value: u8, revision: u16 }",
        ] {
            let input: DeriveInput =
                syn::parse_str(&format!("enum Domain {{ {variant}, #[rca(skip)] Other }}"))
                    .unwrap();
            assert_eq!(
                projection_match_arm_counts(expand(&input).unwrap()),
                [2, 2],
                "shared and mutable state mismatches: {variant}",
            );
            assert_eq!(
                projection_match_arm_counts(crate::action_paths::expand(&input).unwrap()),
                [2],
                "action mismatch: {variant}",
            );
        }
        for variant in [
            "#[rca(keyed)] Ready(rca::KeyedAction<u8, u16>)",
            "#[rca(keyed)] Ready { value: rca::KeyedAction<u8, u16> }",
        ] {
            let input: DeriveInput =
                syn::parse_str(&format!("enum Domain {{ {variant}, #[rca(skip)] Other }}"))
                    .unwrap();
            assert_eq!(
                projection_match_arm_counts(crate::action_paths::expand(&input).unwrap()),
                [2],
                "keyed action mismatch: {variant}",
            );
        }
    }

    #[test]
    fn projection_lifetime_selects_the_first_free_name_without_changing_source() {
        for (source, expected) in [
            ("", "'__rca"),
            ("<'other, T, const N: usize>", "'__rca"),
            ("<'__rca>", "'__rca_1"),
            ("<'__rca, '__rca_1, '__rca_2>", "'__rca_3"),
            ("<'__rca_2, '__rca, '__rca_1>", "'__rca_3"),
            ("<'__rca, '__rca_2>", "'__rca_1"),
            ("<'__rca, '__rca_1, T, const N: usize>", "'__rca_2"),
            (
                "<'__rca, '__rca_0, '__rca_01, '__rca_65535, '__rca_65536>",
                "'__rca_1",
            ),
        ] {
            let generics: syn::Generics = syn::parse_str(source).unwrap();
            let before = generics.to_token_stream().to_string();
            let lifetime = super::projection_lifetime(&generics);
            assert_eq!(lifetime.to_token_stream().to_string(), expected, "{source}");
            assert!(
                generics
                    .lifetimes()
                    .all(|parameter| parameter.lifetime.ident != lifetime.ident),
                "the projection lifetime must not capture a caller lifetime: {source}",
            );
            assert_eq!(generics.to_token_stream().to_string(), before);
        }
    }

    #[test]
    fn projection_lifetime_avoids_nested_binders_and_generated_view_lifetimes() {
        let input: DeriveInput = syn::parse_quote! {
            struct State<F>
            where
                F: for<'__rca_1> Fn(&'__rca_1 str),
            {
                callback: for<'__rca> fn(&'__rca str),
                generic: F,
            }
        };
        let before = input.to_token_stream().to_string();
        let lifetime = super::projection_lifetime(&input);
        assert_eq!(lifetime.to_string(), "'__rca_2");
        let mut view_generics = input.generics.clone();
        view_generics.params.insert(0, syn::parse_quote!(#lifetime));
        assert_eq!(
            super::projection_lifetime(&quote!(#input #view_generics)).to_string(),
            "'__rca_3"
        );
        assert_eq!(input.to_token_stream().to_string(), before);
    }

    #[test]
    fn item_struct_binding_scan_preserves_nested_and_raw_identifier_collisions() {
        let storage: syn::ItemStruct = syn::parse_quote! {
            #[allow(__rca_state)]
            struct Feature<__rca_action, R>
            where
                __rca_action: Trait<__rca_context>,
            {
                body: R,
                r#__rca_reducer_stage_0: bool,
                marker: core::marker::PhantomData<fn() -> __rca_state_>,
            }
        };
        let round_trip: DeriveInput = syn::parse2(quote!(#storage)).unwrap();
        for (base, expected) in [
            ("__rca_state", "__rca_state__"),
            ("__rca_action", "__rca_action_"),
            ("__rca_context", "__rca_context_"),
            ("__rca_reducer_stage_0", "__rca_reducer_stage_0_"),
            ("__rca_reducer_stage_1", "__rca_reducer_stage_1"),
        ] {
            let direct = super::fresh_binding(&storage, base);
            assert_eq!(direct.to_string(), expected);
            assert_eq!(
                direct.to_string(),
                super::fresh_binding(&round_trip, base).to_string(),
                "direct storage and former round-trip must find the same collisions",
            );
        }
    }

    #[test]
    fn conditional_aggregate_variants_gate_markers_constructors_and_mutable_impls() {
        fn assert_conditions(attributes: &[syn::Attribute]) {
            let actual: Vec<_> = attributes
                .iter()
                .map(|attribute| attribute.to_token_stream().to_string())
                .collect();
            assert_eq!(
                actual.get(..2).expect("both conditions must be retained"),
                [
                    quote!(#[cfg(feature = "selected")]).to_string(),
                    quote!(#[cfg_attr(feature = "guarded", cfg(feature = "payload"))]).to_string(),
                ],
            );
            assert!(!actual.contains(&quote!(#[doc = "source variant only"]).to_string()));
        }

        for fields in [quote!((u8, u16)), quote!({ value: u8, revision: u16 })] {
            let input: DeriveInput = syn::parse_quote! {
                enum Domain {
                    #[cfg(feature = "selected")]
                    #[cfg_attr(feature = "guarded", cfg(feature = "payload"))]
                    #[doc = "source variant only"]
                    Ready #fields,
                }
            };
            for mutable in [false, true] {
                let expanded = if mutable {
                    expand(&input)
                } else {
                    crate::action_paths::expand(&input)
                }
                .unwrap();
                let output = syn::parse2::<syn::File>(expanded).unwrap();
                let [
                    syn::Item::Struct(_),
                    syn::Item::Struct(marker),
                    syn::Item::Const(gate),
                    syn::Item::Impl(paths),
                ] = output.items.as_slice()
                else {
                    panic!(
                        "expected a namespace, aggregate marker, gated implementations and constructors"
                    );
                };
                assert_conditions(&marker.attrs);
                assert_conditions(&gate.attrs);
                let [syn::ImplItem::Fn(constructor)] = paths.items.as_slice() else {
                    panic!("expected exactly the selected case constructor");
                };
                assert_eq!(constructor.sig.ident, "ready");
                assert_conditions(&constructor.attrs);
                let syn::Expr::Block(block) = gate.expr.as_ref() else {
                    panic!("aggregate implementations must live inside the conditional block");
                };
                let implementations: Vec<_> = block
                    .block
                    .stmts
                    .iter()
                    .filter_map(|statement| match statement {
                        syn::Stmt::Item(syn::Item::Impl(implementation)) => Some(implementation),
                        _ => None,
                    })
                    .collect();
                let traits: Vec<_> = implementations
                    .iter()
                    .map(|implementation| {
                        implementation
                            .trait_
                            .as_ref()
                            .unwrap()
                            .0
                            .segments
                            .last()
                            .unwrap()
                            .ident
                            .to_string()
                    })
                    .collect();
                if mutable {
                    assert_eq!(traits, ["AggregateCase", "MutableAggregateCase"]);
                    assert_conditions(&implementations[1].attrs);
                } else {
                    assert_eq!(traits, ["AggregateCase"]);
                }
            }
        }
    }

    fn rejects(source: &str, diagnostic: &str) {
        let input = syn::parse_str::<DeriveInput>(source).expect("valid Rust item syntax");
        let error = expand(&input).expect_err("invalid path contract must be rejected");
        assert_eq!(error.to_string(), diagnostic, "{source}");
    }

    // Missing validation must not silently accept ambiguous projection instructions.
    #[test]
    fn raw_projection_helpers_match_canonical_spelling() {
        for option in ["required", "optional", "keyed", "skip"] {
            let canonical = syn::parse_str::<DeriveInput>(&format!(
                "struct State {{ #[rca({option})] value: u8 }}"
            ))
            .unwrap();
            for attribute in [
                format!("#[r#rca({option})]"),
                format!("#[rca(r#{option})]"),
                format!("#[r#rca(r#{option})]"),
            ] {
                let raw = syn::parse_str::<DeriveInput>(&format!(
                    "struct State {{ {attribute} value: u8 }}"
                ))
                .unwrap();
                assert_eq!(
                    expand(&raw).unwrap().to_string(),
                    expand(&canonical).unwrap().to_string(),
                    "{attribute}"
                );
            }
        }
    }

    #[test]
    fn raw_projection_helpers_preserve_rejection_precedence() {
        for (source, diagnostic) in [
            (
                "struct State { #[rca(required, r#required)] value: u8 }",
                "duplicate rca option `required`",
            ),
            (
                "struct State { #[r#rca(r#optional)] #[rca(keyed)] value: u8 }",
                "conflicting rca options `optional` and `keyed`",
            ),
            (
                "struct State { #[rca(required, r#required = 1)] value: u8 }",
                "rca options do not take values",
            ),
            (
                "#[r#rca(r#skip)] struct State { value: u8 }",
                "rca helper attributes are only supported on struct fields or enum variants",
            ),
            (
                "enum State { #[rca(skip)] Hidden(#[r#rca(r#required)] u8) }",
                "rca helper attributes on enum state belong on variants, not payload fields",
            ),
            (
                "enum State { #[r#rca(r#required)] Ready(u8) }",
                "StatePaths enum variants only support #[rca(skip)]",
            ),
        ] {
            rejects(source, diagnostic);
        }
    }

    #[test]
    fn projection_options_reject_malformed_duplicate_and_conflicting_attributes() {
        for (attribute, diagnostic) in [
            (
                "#[rca]",
                "rca helper attributes require one option in parentheses",
            ),
            (
                r#"#[rca = "required"]"#,
                "rca helper attributes require one option in parentheses",
            ),
            ("#[rca()]", "rca helper attributes require one option"),
            (
                "#[rca(unknown)]",
                "unknown rca option; expected `required`, `optional`, `keyed`, or `skip`",
            ),
            ("#[rca(required = 1)]", "rca options do not take values"),
            ("#[rca(optional())]", "rca options do not take values"),
            (
                "#[rca(required, required)]",
                "duplicate rca option `required`",
            ),
            (
                "#[rca(optional, optional)]",
                "duplicate rca option `optional`",
            ),
            ("#[rca(keyed, keyed)]", "duplicate rca option `keyed`"),
            ("#[rca(skip, skip)]", "duplicate rca option `skip`"),
            (
                "#[rca(required, optional)]",
                "conflicting rca options `required` and `optional`",
            ),
            (
                "#[rca(optional)] #[rca(keyed)]",
                "conflicting rca options `optional` and `keyed`",
            ),
        ] {
            rejects(
                &format!("struct State {{ {attribute} value: u8 }}"),
                diagnostic,
            );
        }

        rejects(
            "enum State { #[rca()] Child(u8) }",
            "rca helper attributes require one option",
        );
    }

    // Rejection must identify the misplaced helper or unsupported domain shape.
    #[test]
    fn invalid_locations_and_shapes_report_their_own_diagnostics() {
        for (source, diagnostic) in [
            (
                "#[rca(skip)] struct State { value: u8 }",
                "rca helper attributes are only supported on struct fields or enum variants",
            ),
            (
                "struct State;",
                "StatePaths can only derive paths from named struct fields",
            ),
            (
                "struct State(u8);",
                "StatePaths can only derive paths from named struct fields",
            ),
            (
                "union State { value: u8 }",
                "StatePaths can only be derived for a named struct or enum",
            ),
            (
                "enum State { #[rca(required)] Child(u8) }",
                "StatePaths enum variants only support #[rca(skip)]",
            ),
            (
                "enum State { #[rca(optional)] Child(Option<u8>) }",
                "StatePaths enum variants only support #[rca(skip)]",
            ),
            (
                "enum State { #[rca(keyed)] Child(u8) }",
                "StatePaths enum variants only support #[rca(skip)]",
            ),
            (
                "enum State { FooBar(u8), Foo_Bar(u8) }",
                "StatePaths variants `FooBar` and `Foo_Bar` both produce path constructor `foo_bar`",
            ),
        ] {
            rejects(source, diagnostic);
        }
    }

    // Skipping one member must not suppress an explicitly selected sibling.
    #[test]
    fn skip_omits_only_the_selected_field_or_variant() {
        for (source, expected) in [
            (
                "struct State { #[rca(required)] visible: u8, #[rca(skip)] hidden: u8 }",
                "visible",
            ),
            ("enum State { #[rca(skip)] Hidden(u8), Ready(u8) }", "ready"),
        ] {
            let input = syn::parse_str::<DeriveInput>(source).unwrap();
            let output = syn::parse2::<syn::File>(expand(&input).unwrap()).unwrap();
            let constructors: Vec<_> = output
                .items
                .into_iter()
                .filter_map(|item| {
                    if let syn::Item::Impl(item) = item {
                        Some(item.items)
                    } else {
                        None
                    }
                })
                .flatten()
                .filter_map(|item| {
                    if let syn::ImplItem::Fn(item) = item {
                        Some(item.sig.ident.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(constructors, [expected], "{source}");
        }
    }

    // Named cases use the field name and retain the source domain in both borrows.
    #[test]
    fn named_case_rewrites_associated_payload_for_shared_and_mutable_access() {
        let input: DeriveInput = syn::parse_quote! {
            enum State { Ready { value: Self::Value } }
        };
        let syn::Data::Enum(data) = &input.data else {
            unreachable!()
        };
        let (method, constructor, case) = super::expand_variant(
            &super::VariantContext {
                input: &input,
                variant: &data.variants[0],
                fallback: &proc_macro2::TokenStream::new(),
            },
            &syn::parse_quote!(State),
            &syn::parse_quote!('__rca),
            &quote!(::rca),
        )
        .unwrap()
        .unwrap();
        assert_eq!(method, "value");
        assert!(
            case.is_none(),
            "one payload must not become an aggregate case"
        );
        let constructor = syn::parse2::<syn::ImplItemFn>(constructor).unwrap();
        assert_eq!(
            constructor.sig.output.to_token_stream().to_string(),
            quote! {
                -> ::rca::CaseStatePath<
                    State,
                    State::Value,
                    impl for<'__rca> ::core::ops::Fn(&'__rca State) -> ::core::option::Option<&'__rca State::Value>,
                    impl for<'__rca> ::core::ops::Fn(&'__rca mut State) -> ::core::option::Option<&'__rca mut State::Value>,
                >
            }.to_string(),
        );
        assert_eq!(
            constructor
                .block
                .stmts
                .last()
                .unwrap()
                .to_token_stream()
                .to_string(),
            quote! {
                ::rca::CaseStatePath::new(
                    |state: &State| {
                        match state {
                            State::Ready { value: child } => ::core::option::Option::Some(child),
                        }
                    },
                    |state: &mut State| {
                        match state {
                            State::Ready { value: child } => ::core::option::Option::Some(child),
                        }
                    },
                )
            }
            .to_string(),
        );
    }

    #[test]
    fn enum_payload_helpers_are_rejected_even_on_skipped_variants() {
        for input in [
            syn::parse_quote!(
                enum State {
                    Child(#[rca(optional)] Option<u8>),
                }
            ),
            syn::parse_quote!(
                enum State {
                    Child {
                        #[rca(skip)]
                        child: u8,
                    },
                }
            ),
            syn::parse_quote!(
                enum State {
                    #[rca(skip)]
                    Child(#[rca(skip)] u8),
                }
            ),
        ] {
            assert_eq!(
                expand(&input).unwrap_err().to_string(),
                "rca helper attributes on enum state belong on variants, not payload fields",
                "payload helpers must not be ignored",
            );
        }
    }

    #[test]
    fn public_path_expansion_matches_snapshot() {
        let input: DeriveInput = syn::parse_quote! {
            pub struct AppState<Id>
            where
                Id: Eq,
            {
                child: ChildState,
                #[rca(optional)]
                detail: Option<DetailState>,
                #[rca(keyed)]
                rows: rca::KeyedState<Id, RowState>,
            }
        };

        let actual = expand(&input).expect("expansion succeeds");
        let expanded: syn::File = syn::parse2(actual.clone()).expect("valid expansion");
        let syn::Item::Mod(namespace) = &expanded.items[0] else {
            panic!("expected the type-only namespace module");
        };
        assert!(matches!(
            namespace
                .content
                .as_ref()
                .map(|(_, items)| items.as_slice()),
            Some([syn::Item::Struct(_)])
        ));
        insta::assert_snapshot!(actual.to_string());
    }

    #[test]
    fn const_only_namespace_preserves_colliding_name_and_braced_argument() {
        let input: DeriveInput = syn::parse_quote! {
            pub struct Packet<const PacketPaths: usize> {
                bytes: [u8; PacketPaths],
            }
        };
        let parts = super::expand_parts(&input, None).expect("const-only expansion succeeds");
        let namespace: syn::ItemStruct =
            syn::parse2(parts.namespace).expect("const-only path namespace");
        assert_eq!(namespace.ident, "PacketPaths");
        assert_eq!(
            namespace.generics.to_token_stream().to_string(),
            quote!(<const PacketPaths: usize>).to_string(),
        );
        let syn::Fields::Unnamed(fields) = namespace.fields else {
            panic!("const-only namespace must retain its state type");
        };
        assert_eq!(fields.unnamed.len(), 1);
        let expected_type: syn::Type =
            syn::parse_quote!(::core::marker::PhantomData<fn() -> Packet<{ PacketPaths }>>);
        assert_eq!(
            fields.unnamed[0].ty.to_token_stream().to_string(),
            expected_type.to_token_stream().to_string(),
        );

        let constructors: syn::ItemImpl =
            syn::parse2(parts.constructors).expect("const-only constructor implementation");
        assert_eq!(
            constructors.generics.to_token_stream().to_string(),
            quote!(<const PacketPaths: usize>).to_string(),
        );
        assert_eq!(
            constructors.self_ty.to_token_stream().to_string(),
            quote!(PacketPaths<{ PacketPaths }>).to_string(),
        );
        let [syn::ImplItem::Fn(constructor)] = constructors.items.as_slice() else {
            panic!("one field must produce one path constructor");
        };
        assert_eq!(constructor.sig.ident, "bytes");
    }

    #[test]
    fn keyword_case_expansion_matches_snapshot() {
        let input: DeriveInput = syn::parse_quote! {
            pub enum State {
                Gen(ChildState),
            }
        };

        let actual = expand(&input).expect("expansion succeeds");
        insta::assert_snapshot!(actual.to_string());
    }
}
