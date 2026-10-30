//! Generates borrowed action projections and owned embedders from enum variants.
//!
//! Aggregate cases reuse the path generator's typed views. A reducer may supply
//! a conditional fallback before rustc has removed disabled variants.

use std::collections::HashMap;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Attribute, Data, DeriveInput, Fields, Ident, Lifetime, Type, Variant};

use crate::{
    canonical_ident, path_is_ident, rca_path, snake_case_ident,
    state_paths::{
        CaseItems, PathExpansion, VariantContext, aggregate_variant, declaration_generics,
        fresh_binding, generic_namespace, projection_lifetime, source_generics, source_type,
        type_arguments,
    },
    type_reference,
};

/// Selects a variant adapter instead of the ordinary payload projection.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Projection {
    /// Splits a keyed payload into its identity and child action.
    Keyed,
    /// Omits the constructor without skipping helper validation.
    Skip,
}

impl Projection {
    /// Returns the canonical spelling used by duplicate-option diagnostics.
    const fn name(self) -> &'static str {
        match self {
            Self::Keyed => "keyed",
            Self::Skip => "skip",
        }
    }
}

/// Expands a derive input after rustc has applied its conditional attributes.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    Ok(expand_parts(input, None, input)?.into_tokens())
}

/// Separates the namespace, aggregate views and constructors for reducer output.
///
/// `enum_fallback` replaces variant-count inference when the reducer still has
/// conditional variants that rustc has not removed.
/// Alias names use the whole declaration, even when only one member is lowered.
pub(crate) fn expand_parts(
    input: &DeriveInput,
    enum_fallback: Option<&TokenStream>,
    declaration: &DeriveInput,
) -> syn::Result<PathExpansion> {
    validate_type_attributes(&input.attrs)?;
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "ActionPaths can only be derived for an enum",
        ));
    };

    let rca = rca_path()?;
    let paths = paths_ident(&input.ident)?;
    let (action, action_alias) = type_reference(
        declaration,
        &input.generics,
        &input.generics,
        &input.ident,
        "ActionPaths",
    );
    let (paths_reference, paths_alias) = type_reference(
        declaration,
        &input.generics,
        &input.generics,
        &paths,
        "ActionPaths",
    );
    let visibility = &input.vis;
    let generics = &input.generics;
    let lifetime = projection_lifetime(input);
    let action_generics = type_arguments(generics);
    let action_type: Type = syn::parse_quote!(#action #action_generics);
    let rewritten_generics = source_generics(generics, &action_type);
    let fallback = enum_fallback.cloned().unwrap_or_else(|| {
        if data.variants.len() > 1 {
            quote!(_ => ::core::option::Option::None,)
        } else {
            TokenStream::new()
        }
    });
    let (constructors, cases) = expand_variants(
        &data.variants,
        &action_type,
        &lifetime,
        &rca,
        input,
        &action,
        &fallback,
    )?;
    let (impl_generics, _, where_clause) = rewritten_generics.split_for_impl();
    let type_generics = type_arguments(&rewritten_generics);
    let (declaration_generics, declaration_where_clause) =
        declaration_generics(generics, &action_type);
    let namespace = if generics.type_params().next().is_some() {
        generic_namespace(input, &paths, &action_type)
    } else if generics.params.is_empty() {
        quote!(
            /// Constructs borrowed projections and owned embedders for the source action.
            #[derive(::core::fmt::Debug)]
            #visibility struct #paths #declaration_generics #declaration_where_clause;
        )
    } else {
        quote!(
            /// Constructs borrowed projections and owned embedders for the source action.
            #[derive(::core::fmt::Debug)]
            #visibility struct #paths #declaration_generics(
                ::core::marker::PhantomData<fn() -> #action_type>
            ) #declaration_where_clause;
        )
    };

    let constructors = quote! {
        #[automatically_derived]
        impl #impl_generics #paths_reference #type_generics #where_clause {
            #(#constructors)*
        }
    };
    Ok(PathExpansion {
        namespace: quote!(#action_alias #paths_alias #[automatically_derived] #namespace),
        cases,
        constructors,
    })
}

/// Collects constructors in source order and rejects canonical name collisions.
fn expand_variants(
    variants: &syn::punctuated::Punctuated<Variant, syn::token::Comma>,
    action: &Type,
    lifetime: &Lifetime,
    rca: &TokenStream,
    input: &DeriveInput,
    root: &Ident,
    fallback: &TokenStream,
) -> syn::Result<(Vec<TokenStream>, Vec<[TokenStream; 2]>)> {
    let mut constructors = Vec::new();
    let mut cases = Vec::new();
    let mut methods = HashMap::new();
    for variant in variants {
        let Some((method, constructor, case)) = expand_variant(
            &VariantContext {
                input,
                root,
                variant,
                fallback,
            },
            action,
            lifetime,
            rca,
        )?
        else {
            continue;
        };
        let method_name = canonical_ident(&method);
        if let Some(previous) = methods.insert(method_name, &variant.ident) {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                format!(
                    "ActionPaths variants `{previous}` and `{}` both produce path constructor `{}`",
                    variant.ident,
                    canonical_ident(&method),
                ),
            ));
        }
        constructors.push(constructor);
        cases.extend(case);
    }
    Ok((constructors, cases))
}

/// Validates a variant and emits its single-payload or aggregate constructor.
///
/// Helper placement and method names are checked before shape-specific errors;
/// skipping a constructor does not make helpers valid on its payload fields.
fn expand_variant(
    context: &VariantContext<'_>,
    action: &Type,
    lifetime: &Lifetime,
    rca: &TokenStream,
) -> syn::Result<Option<(Ident, TokenStream, Option<CaseItems>)>> {
    let input = context.input;
    let variant = context.variant;
    let fallback = context.fallback;
    let action_ident = context.root;
    let visibility = &input.vis;
    let projection = projection(&variant.attrs)?;
    validate_field_attributes(&variant.fields)?;
    let keyed = match projection {
        None => false,
        Some(Projection::Keyed) => true,
        Some(Projection::Skip) => return Ok(None),
    };
    let action_binding = fresh_binding(input, "action");
    let payload_binding = fresh_binding(input, "payload");
    let variant_ident = &variant.ident;
    let method = snake_case_ident(variant_ident)?;
    let mut fields = variant.fields.iter();
    let (payload, pattern) = if let (Some(field), None) = (fields.next(), fields.next()) {
        let payload = source_type(&field.ty, action);
        let pattern = if let Some(field_ident) = &field.ident {
            quote!(#action_ident::#variant_ident { #field_ident: #payload_binding })
        } else {
            quote!(#action_ident::#variant_ident(#payload_binding))
        };
        (payload, pattern)
    } else {
        if keyed {
            return Err(syn::Error::new_spanned(
                variant_ident,
                "#[rca(keyed)] requires one KeyedAction payload",
            ));
        }
        let (constructor, case) = aggregate_variant(context, action, &method, lifetime, rca, false);
        return Ok(Some((method, constructor, Some(case))));
    };

    let constructor = if keyed {
        quote! {
            /// Returns a path that borrows or embeds this case's keyed action.
            #visibility fn #method() -> #rca::KeyedActionPath<
                #action,
                <#payload as #rca::KeyedActionPayload>::Id,
                <#payload as #rca::KeyedActionPayload>::Action,
                impl for<#lifetime> ::core::ops::Fn(&#lifetime #action) -> ::core::option::Option<&#lifetime #rca::KeyedAction<<#payload as #rca::KeyedActionPayload>::Id, <#payload as #rca::KeyedActionPayload>::Action>>,
                impl ::core::ops::Fn(#rca::KeyedAction<<#payload as #rca::KeyedActionPayload>::Id, <#payload as #rca::KeyedActionPayload>::Action>) -> #action + ::core::clone::Clone,
            >
            where
                #payload: #rca::KeyedActionPayload,
            {
                #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
                use ::core::mem::{drop as #action_binding, drop as #payload_binding};
                #rca::KeyedActionPath::new(
                    |#action_binding: &#action| {
                        match #action_binding {
                            #pattern => ::core::option::Option::Some(#rca::KeyedActionPayload::as_keyed_action(#payload_binding)),
                            #fallback
                        }
                    },
                    |#payload_binding| {
                        let #payload_binding = #rca::KeyedActionPayload::from_keyed_action(#payload_binding);
                        #pattern
                    },
                )
            }
        }
    } else {
        quote! {
            /// Returns a path that borrows or embeds this case's payload.
            #visibility fn #method() -> #rca::ActionPath<
                #action,
                #payload,
                impl for<#lifetime> ::core::ops::Fn(&#lifetime #action) -> ::core::option::Option<&#lifetime #payload>,
                impl ::core::ops::Fn(#payload) -> #action + ::core::clone::Clone,
            > {
                #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
                use ::core::mem::{drop as #action_binding, drop as #payload_binding};
                #rca::ActionPath::new(
                    |#action_binding: &#action| {
                        match #action_binding {
                            #pattern => ::core::option::Option::Some(#payload_binding),
                            #fallback
                        }
                    },
                    |#payload_binding| #pattern,
                )
            }
        }
    };
    Ok(Some((method, constructor, None)))
}

/// Parses one adapter across all helpers, preserving value-before-duplicate errors.
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
            let next = if path_is_ident(&meta.path, "keyed") {
                Projection::Keyed
            } else if path_is_ident(&meta.path, "skip") {
                Projection::Skip
            } else if path_is_ident(&meta.path, "required") || path_is_ident(&meta.path, "optional")
            {
                return Err(meta.error(
                    "ActionPaths only supports `keyed` and `skip` rca options on enum variants",
                ));
            } else {
                return Err(meta.error("unknown rca option; expected `keyed` or `skip`"));
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

/// Rejects variant-only helpers on the enum itself, before validating its shape.
fn validate_type_attributes(attributes: &[Attribute]) -> syn::Result<()> {
    if let Some(attribute) = attributes
        .iter()
        .find(|attribute| path_is_ident(attribute.path(), "rca"))
    {
        return Err(syn::Error::new_spanned(
            attribute,
            "rca helper attributes are only supported on ActionPaths enum variants",
        ));
    }
    Ok(())
}

/// Rejects helpers on payload fields, including fields of skipped variants.
fn validate_field_attributes(fields: &Fields) -> syn::Result<()> {
    for field in fields {
        if let Some(attribute) = field
            .attrs
            .iter()
            .find(|attribute| path_is_ident(attribute.path(), "rca"))
        {
            return Err(syn::Error::new_spanned(
                attribute,
                "rca helper attributes are only supported on ActionPaths enum variants",
            ));
        }
    }
    Ok(())
}

/// Validates the suffixed name, including malformed names rustc passes to derives.
fn paths_ident(action: &Ident) -> syn::Result<Ident> {
    let mut paths: Ident =
        syn::parse_str(&format!("{}Paths", canonical_ident(action))).map_err(|_parse_error| {
            syn::Error::new_spanned(
                action,
                "ActionPaths cannot derive a valid path namespace name",
            )
        })?;
    paths.set_span(action.span());
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use quote::{ToTokens, quote};
    use syn::DeriveInput;

    use super::expand;

    #[test]
    fn namespace_suffix_uses_canonical_raw_and_unicode_names() {
        for (root, namespace) in [
            ("Action", "ActionPaths"),
            ("r#type", "typePaths"),
            ("r#Événement", "ÉvénementPaths"),
        ] {
            let root = syn::parse_str(root).unwrap();
            assert_eq!(super::paths_ident(&root).unwrap(), namespace);
        }
    }

    fn rejects(source: &str, diagnostic: &str) {
        let input = syn::parse_str::<DeriveInput>(source).expect("valid Rust item syntax");
        let error = expand(&input).expect_err("invalid path contract must be rejected");
        assert_eq!(error.to_string(), diagnostic, "{source}");
    }

    #[test]
    fn raw_projection_spellings_select_the_same_adapter() {
        for (source, expected) in [
            ("#[r#rca(keyed)]", Some("keyed")),
            ("#[rca(r#keyed)]", Some("keyed")),
            ("#[r#rca(r#skip)]", Some("skip")),
            ("#[other::rca(keyed)]", None),
        ] {
            let input: DeriveInput = syn::parse_str(&format!("{source} enum Action {{}}"))
                .expect("valid helper attribute syntax");
            let actual = super::projection(&input.attrs)
                .expect("raw spelling does not change the selected projection");
            assert_eq!(actual.map(super::Projection::name), expected, "{source}");
        }
    }

    #[test]
    fn raw_projection_options_keep_existing_validation_and_precedence() {
        for (attribute, diagnostic) in [
            ("#[rca(keyed, r#keyed)]", "duplicate rca option `keyed`"),
            ("#[r#rca(skip)] #[rca(skip)]", "duplicate rca option `skip`"),
            (
                "#[rca(r#keyed, skip)]",
                "conflicting rca options `keyed` and `skip`",
            ),
            ("#[r#rca()]", "rca helper attributes require one option"),
            ("#[rca(r#keyed = 1)]", "rca options do not take values"),
            (
                "#[rca(r#optional)]",
                "ActionPaths only supports `keyed` and `skip` rca options on enum variants",
            ),
            (
                "#[rca(other::keyed)]",
                "unknown rca option; expected `keyed` or `skip`",
            ),
        ] {
            rejects(
                &format!("enum Action {{ {attribute} Child(u8) }}"),
                diagnostic,
            );
        }
    }

    #[test]
    fn raw_helpers_remain_invalid_on_roots_and_payload_fields() {
        for source in [
            "#[r#rca(skip)] enum Action { Child(u8) }",
            "enum Action { Child(#[r#rca(skip)] u8) }",
            "enum Action { #[rca(skip)] Child { #[r#rca(skip)] value: u8 } }",
        ] {
            rejects(
                source,
                "rca helper attributes are only supported on ActionPaths enum variants",
            );
        }
    }

    // Unsupported or ambiguous options must not emit a differently routed action.
    #[test]
    fn projection_options_reject_malformed_duplicate_and_conflicting_attributes() {
        for (attribute, diagnostic) in [
            (
                "#[rca]",
                "rca helper attributes require one option in parentheses",
            ),
            (
                r#"#[rca = "keyed"]"#,
                "rca helper attributes require one option in parentheses",
            ),
            ("#[rca()]", "rca helper attributes require one option"),
            (
                "#[rca(required)]",
                "ActionPaths only supports `keyed` and `skip` rca options on enum variants",
            ),
            (
                "#[rca(optional)]",
                "ActionPaths only supports `keyed` and `skip` rca options on enum variants",
            ),
            (
                "#[rca(unknown)]",
                "unknown rca option; expected `keyed` or `skip`",
            ),
            ("#[rca(keyed = 1)]", "rca options do not take values"),
            ("#[rca(skip())]", "rca options do not take values"),
            ("#[rca(keyed, keyed)]", "duplicate rca option `keyed`"),
            ("#[rca(skip, skip)]", "duplicate rca option `skip`"),
            (
                "#[rca(keyed, skip)]",
                "conflicting rca options `keyed` and `skip`",
            ),
            (
                "#[rca(skip)] #[rca(keyed)]",
                "conflicting rca options `skip` and `keyed`",
            ),
        ] {
            rejects(
                &format!("enum Action {{ {attribute} Child(u8) }}"),
                diagnostic,
            );
        }
    }

    // Payload helpers remain invalid even when the enclosing variant is skipped.
    #[test]
    fn invalid_locations_and_shapes_report_their_own_diagnostics() {
        for (source, diagnostic) in [
            (
                "struct Action;",
                "ActionPaths can only be derived for an enum",
            ),
            (
                "union Action { value: u8 }",
                "ActionPaths can only be derived for an enum",
            ),
            (
                "#[rca(skip)] enum Action { Child(u8) }",
                "rca helper attributes are only supported on ActionPaths enum variants",
            ),
            (
                "enum Action { Child(#[rca(skip)] u8) }",
                "rca helper attributes are only supported on ActionPaths enum variants",
            ),
            (
                "enum Action { #[rca(skip)] Child { #[rca(skip)] value: u8 } }",
                "rca helper attributes are only supported on ActionPaths enum variants",
            ),
            (
                "enum Action { #[rca(keyed)] Child }",
                "#[rca(keyed)] requires one KeyedAction payload",
            ),
            (
                "enum Action { #[rca(keyed)] Child(u8, u8) }",
                "#[rca(keyed)] requires one KeyedAction payload",
            ),
            (
                "enum Action { FooBar(u8), Foo_Bar(u8) }",
                "ActionPaths variants `FooBar` and `Foo_Bar` both produce path constructor `foo_bar`",
            ),
        ] {
            rejects(source, diagnostic);
        }
    }

    #[test]
    fn forbidden_keyed_aggregate_method_precedes_payload_arity_diagnostic() {
        for payload in ["", "()", "{}", "(u8, u16)", "{ first: u8, second: u16 }"] {
            rejects(
                &format!("enum Action {{ #[rca(keyed)] Crate{payload} }}"),
                "RCA derives cannot generate identifier `crate` because it is not a legal raw Rust identifier",
            );
        }
    }

    // Named actions must extract and embed the same associated payload and variant.
    #[test]
    fn named_case_rewrites_associated_payload_for_extraction_and_embedding() {
        let input: DeriveInput = syn::parse_quote! {
            enum Action { Ready { value: Self::Value } }
        };
        let syn::Data::Enum(data) = &input.data else {
            unreachable!()
        };
        let (method, constructor, case) = super::expand_variant(
            &super::VariantContext {
                input: &input,
                root: &input.ident,
                variant: &data.variants[0],
                fallback: &proc_macro2::TokenStream::new(),
            },
            &syn::parse_quote!(Action),
            &syn::parse_quote!('__rca),
            &quote!(::rca),
        )
        .unwrap()
        .unwrap();
        assert_eq!(method, "ready");
        assert!(
            case.is_none(),
            "one payload must not become an aggregate case"
        );
        let constructor = syn::parse2::<syn::ImplItemFn>(constructor).unwrap();
        assert_eq!(
            constructor.sig.output.to_token_stream().to_string(),
            quote! {
                -> ::rca::ActionPath<
                    Action,
                    Action::Value,
                    impl for<'__rca> ::core::ops::Fn(&'__rca Action) -> ::core::option::Option<&'__rca Action::Value>,
                    impl ::core::ops::Fn(Action::Value) -> Action + ::core::clone::Clone,
                >
            }.to_string(),
        );
        assert_eq!(
            constructor.block.stmts.last().unwrap().to_token_stream().to_string(),
            quote! {
                ::rca::ActionPath::new(
                    |action: &Action| {
                        match action {
                            Action::Ready { value: payload } => ::core::option::Option::Some(payload),
                        }
                    },
                    |payload| Action::Ready { value: payload },
                )
            }.to_string(),
        );
    }

    #[test]
    fn public_path_expansion_matches_snapshot() {
        let input: DeriveInput = syn::parse_quote! {
            pub enum AppAction<Id> {
                Child(ChildAction),
                #[rca(keyed)]
                Row(rca::KeyedAction<Id, RowAction>),
                SetName { name: String },
                Parent,
            }
        };

        let actual = expand(&input).expect("expansion succeeds");
        insta::assert_snapshot!(actual.to_string());

        let input: DeriveInput = syn::parse_quote! {
            pub enum Packet<const PacketPaths: usize> {
                Bytes([u8; PacketPaths]),
            }
        };
        let parts =
            super::expand_parts(&input, None, &input).expect("const-only expansion succeeds");
        let namespace: syn::ItemStruct =
            syn::parse2(parts.namespace).expect("const-only path namespace");
        assert_eq!(namespace.ident, "PacketPaths");
        assert_eq!(
            namespace.generics.to_token_stream().to_string(),
            quote!(<const PacketPaths: usize>).to_string(),
        );
        let syn::Fields::Unnamed(fields) = namespace.fields else {
            panic!("const-only namespace must retain its action type");
        };
        assert_eq!(fields.unnamed.len(), 1);
        let field = fields.unnamed.first().unwrap();
        let expected_type: syn::Type =
            syn::parse_quote!(::core::marker::PhantomData<fn() -> Packet<{ PacketPaths }>>);
        assert_eq!(
            field.ty.to_token_stream().to_string(),
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
            panic!("one payload must produce one path constructor");
        };
        assert_eq!(constructor.sig.ident, "bytes");
    }
}
