//! Generates owned snapshot values and their capture implementations.
//!
//! Companion types depend on captured field types, not source lifetimes or handles.
//! Custom capture functions remain responsible for independence from live state.

use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::{
    Attribute, Data, DeriveInput, Field, Fields, Generics, Ident, LitStr, Member, Path, Type,
    TypePath,
    spanned::Spanned,
    visit_mut::{self, VisitMut},
};

use crate::state_paths::{fresh_binding, source_generics, source_type, type_arguments};
use crate::{canonical_ident, path_is_ident, rca_path};

/// Stores location-checked helper syntax without resolving caller types or functions.
#[derive(Default)]
struct Options {
    /// Explicit companion name, mutually exclusive with an existing destination.
    name: Option<Ident>,
    /// Existing named-field destination whose traits remain caller-owned.
    into: Option<TypePath>,
    /// Custom function that receives a shared reference to the source field.
    with: Option<Path>,
    /// Declared custom result type, paired with `with` and checked syntactically.
    value: Option<Type>,
    /// Non-empty explanation for a field excluded from captured history.
    omit: Option<LitStr>,
    /// Redaction location for diagnostics, without changing captured data or equality.
    redact: Option<proc_macro2::Span>,
}

/// Selects the helper grammar permitted at each source location.
#[derive(Clone, Copy)]
enum Location {
    /// A source declaration can rename its companion or select an existing destination.
    Item,
    /// A field can customise capture, omit history or redact debug output.
    Field,
    /// A variant retains its identity and accepts no snapshot options.
    Variant,
}

/// Parses helper options in source order and rejects incompatible combinations.
///
/// Duplicate keys fail before their replacement values are parsed. Independent
/// source locations are checked separately by `expand`, which combines their errors.
fn options(attributes: &[Attribute], location: Location) -> syn::Result<Options> {
    let mut result = Options::default();
    let mut seen = std::collections::HashSet::new();
    for attribute in attributes
        .iter()
        .filter(|attribute| path_is_ident(attribute.path(), "snapshot"))
    {
        attribute.parse_nested_meta(|meta| {
            let key = meta.path.get_ident().map(canonical_ident).unwrap_or_default();
            if !seen.insert(key.clone()) {
                return Err(meta.error(format!("duplicate snapshot option `{key}`")));
            }
            match (&location, key.as_str()) {
                (Location::Item, "name") => result.name = Some(meta.value()?.parse()?),
                (Location::Item, "into") => {
                    let ty: TypePath = meta.value()?.parse()?;
                    if ty.qself.is_some() {
                        return Err(meta.error("snapshot `into` requires a named struct path, not a qualified associated type"));
                    }
                    result.into = Some(ty);
                }
                (Location::Field, "with") => result.with = Some(meta.value()?.parse()?),
                (Location::Field, "value") => result.value = Some(meta.value()?.parse()?),
                (Location::Field, "omit") => {
                    let reason: LitStr = meta.value()?.parse()?;
                    if reason.value().trim().is_empty() {
                        return Err(meta.error("snapshot omission requires a non-empty reason"));
                    }
                    result.omit = Some(reason);
                }
                (Location::Field, "redact") => result.redact = Some(meta.path.span()),
                (Location::Item, _) => return Err(meta.error("expected snapshot item option `name` or `into`")),
                (Location::Field, _) => return Err(meta.error("expected snapshot field option `with`, `value`, `omit` or `redact`")),
                (Location::Variant, _) => return Err(meta.error("snapshot options are not supported on variants; case identity is always captured")),
            }
            Ok(())
        })?;
    }
    if result.name.is_some()
        && let Some(destination) = &result.into
    {
        return Err(syn::Error::new_spanned(
            destination,
            "snapshot `name` and `into` are mutually exclusive",
        ));
    }
    if result.with.is_some() != result.value.is_some() {
        return Err(syn::Error::new_spanned(
            quote!(#(#attributes)*),
            "snapshot `with` and `value` must be supplied together",
        ));
    }
    if let Some(reason) = &result.omit
        && (result.with.is_some() || result.redact.is_some())
    {
        return Err(syn::Error::new_spanned(
            reason,
            "snapshot `omit` cannot combine with capture or redaction",
        ));
    }
    if let Some(value) = &result.value {
        let mut checker = TypeCheck {
            source: None,
            source_shadowed: false,
            error: None,
        };
        checker.visit_type_mut(&mut value.clone());
        if let Some(error) = checker.error {
            return Err(error);
        }
    }
    Ok(result)
}

/// Rejects direct recursive capture or explicit borrowed custom-value syntax.
///
/// This check does not resolve aliases or prove custom capture independence.
/// Rustc checks trait obligations and the types produced by custom functions.
struct TypeCheck<'a> {
    /// Canonical source name for recursion checks, or `None` for custom-value checks.
    source: Option<&'a str>,
    /// Whether a type parameter hides the source name in its own declaration.
    source_shadowed: bool,
    /// Combined diagnostics for independent invalid type nodes.
    error: Option<syn::Error>,
}

impl VisitMut for TypeCheck<'_> {
    fn visit_type_mut(&mut self, i: &mut Type) {
        let ty = i;
        // A function pointer captures no argument or result storage. Its signature
        // does not borrow data from the source or recursively contain that source.
        if matches!(ty, Type::FnPtr(_)) {
            return;
        }
        // An associated projection is not direct recursive storage. Its trait
        // obligation remains the compiler's responsibility, including aliases.
        if self.source.is_some()
            && let Type::Path(path) = ty
            && path.qself.is_some()
        {
            visit_mut::visit_path_mut(self, &mut path.path);
            return;
        }
        let message = if let Some(source) = self.source {
            match ty {
                Type::Path(p)
                    if p.path.is_ident("Self")
                        || (!self.source_shadowed && path_is_ident(&p.path, source)) =>
                {
                    Some(
                        "recursive snapshot capture requires a finite custom capture or manual implementation",
                    )
                }
                _ => None,
            }
        } else if matches!(ty, Type::Reference(_) | Type::Ptr(_)) {
            Some(
                "snapshot custom values must be owned; use an owned value or a manual implementation",
            )
        } else {
            None
        };
        if let Some(message) = message {
            combine(&mut self.error, syn::Error::new_spanned(&*ty, message));
        } else {
            visit_mut::visit_type_mut(self, ty);
        }
    }

    fn visit_expr_mut(&mut self, _: &mut syn::Expr) {
        // Constant expressions compute type metadata, not captured storage.
        // Their local bindings and borrows remain the compiler's responsibility.
    }
}

/// Retains diagnostic order while accumulating independent validation failures.
fn combine(errors: &mut Option<syn::Error>, error: syn::Error) {
    if let Some(errors) = errors {
        errors.combine(error);
    } else {
        *errors = Some(error);
    }
}

/// Renames only the impl type parameter that hides the source declaration.
///
/// Item, field and variant names, absolute paths and bare value paths stay unchanged.
struct ParameterRename<'a> {
    /// Canonical type-parameter name that also names the source declaration.
    from: &'a str,
    /// Fresh replacement used within the generated implementation.
    to: Ident,
}

impl ParameterRename<'_> {
    /// Rewrites the head of a relative type or associated-function path.
    fn path_head(&self, path: &mut Path) {
        if path.leading_colon.is_none() {
            // Paths originate in parsed Rust syntax and always have a first segment.
            let first = &mut path.segments[0];
            if canonical_ident(&first.ident) == self.from {
                first.ident = self.to.clone();
            }
        }
    }

    /// Rewrites type references in helper syntax without renaming bare capture functions.
    fn options(&mut self, options: &mut Options) {
        if let Some(destination) = &mut options.into {
            self.visit_type_path_mut(destination);
        }
        if let Some(value) = &mut options.value {
            self.visit_type_mut(value);
        }
        if let Some(function) = &mut options.with {
            if function.segments.len() > 1 {
                self.path_head(function);
            }
            self.visit_path_mut(function);
        }
    }
}

impl VisitMut for ParameterRename<'_> {
    fn visit_type_param_mut(&mut self, i: &mut syn::TypeParam) {
        if canonical_ident(&i.ident) == self.from {
            i.ident = self.to.clone();
        }
        visit_mut::visit_type_param_mut(self, i);
    }

    fn visit_type_path_mut(&mut self, i: &mut TypePath) {
        if i.qself.is_none() {
            self.path_head(&mut i.path);
        }
        visit_mut::visit_type_path_mut(self, i);
    }

    fn visit_expr_mut(&mut self, _: &mut syn::Expr) {
        // Stable const expressions in types cannot use outer type parameters.
        // Preserve their independent item, import and value namespaces.
    }
}

/// Links one retained source field to its generated value and pattern binding.
struct CapturedField<'a> {
    /// Original declaration, retained for visibility, attributes and diagnostic spans.
    field: &'a Field,
    /// Source member name or original tuple index, before omitted fields are removed.
    member: Member,
    /// Fresh local name for a shared borrow during capture or debug formatting.
    binding: Ident,
    /// Companion type parameter representing this field's captured value.
    parameter: Ident,
    /// Checked capture and redaction choices for this field.
    options: Options,
    /// Custom output type or the source field's associated snapshot value type.
    value: Type,
    /// Source field type after `Self` substitution and type-parameter renaming.
    source: Type,
}

/// Describes a struct or enum variant while retaining original field positions.
struct Shape<'a> {
    /// Original enum variant, or `None` for a struct.
    variant: Option<&'a syn::Variant>,
    /// Source field layout, including fields omitted from history.
    fields: &'a Fields,
    /// Retained fields in source order, excluding omitted or invalid fields.
    captured: Vec<CapturedField<'a>>,
}

/// Copies documentation and active configuration without copying layout or helper attributes.
fn copied_attributes(attributes: &[Attribute]) -> Vec<&Attribute> {
    attributes
        .iter()
        .filter(|attribute| {
            path_is_ident(attribute.path(), "doc") || path_is_ident(attribute.path(), "cfg")
        })
        .collect()
}

#[expect(
    clippy::too_many_lines,
    reason = "field validation, companion shape and capture implementation share one derived model"
)]
/// Checks capture syntax and emits a companion or an existing-destination implementation.
///
/// Independent field errors are combined before generation. Companion parameters
/// describe captured values, so source lifetimes and unused bounds do not leak.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let mut errors = None;
    let mut opts = match options(&input.attrs, Location::Item) {
        Ok(options) => options,
        Err(error) => {
            combine(&mut errors, error);
            Options::default()
        }
    };
    let shape_inputs: Vec<_> = match &input.data {
        Data::Struct(data) => vec![(None, &data.fields)],
        Data::Enum(data) => data.variants.iter().map(|v| (Some(v), &v.fields)).collect(),
        Data::Union(data) => {
            return Err(syn::Error::new_spanned(
                data.union_token,
                "Snapshot cannot be derived for unions",
            ));
        }
    };
    if let Some(destination) = &opts.into
        && !matches!(&input.data, Data::Struct(data) if matches!(data.fields, Fields::Named(_)))
    {
        combine(
            &mut errors,
            syn::Error::new_spanned(
                destination,
                "snapshot `into` supports only named-field source structs; use a manual implementation",
            ),
        );
    }
    let name = &input.ident;
    let canonical_name = canonical_ident(name);
    let source_shadowed = input
        .generics
        .type_params()
        .any(|p| canonical_ident(&p.ident) == canonical_name);
    let mut rename = source_shadowed.then(|| ParameterRename {
        from: &canonical_name,
        to: fresh_binding(input, "__RcaSnapshotSource"),
    });
    let mut source_parameters = input.generics.clone();
    if let Some(rename) = &mut rename {
        rename.visit_generics_mut(&mut source_parameters);
        rename.options(&mut opts);
    }
    let source_arguments = type_arguments(&source_parameters);
    let source: Type = syn::parse_quote!(#name #source_arguments);
    let rca = rca_path()?;
    let mut generics = source_generics(&source_parameters, &source);
    let mut shapes = Vec::new();
    let mut count = 0;
    let mut omission_docs = Vec::new();
    for (variant, fields) in shape_inputs {
        if let Some(variant) = variant
            && let Err(error) = options(&variant.attrs, Location::Variant)
        {
            combine(&mut errors, error);
        }
        let mut captured = Vec::new();
        for (index, field) in fields.iter().enumerate() {
            let mut options = match options(&field.attrs, Location::Field) {
                Ok(options) => options,
                Err(error) => {
                    combine(&mut errors, error);
                    continue;
                }
            };
            if let Some(reason) = &options.omit {
                let field_name = field
                    .ident
                    .as_ref()
                    .map_or_else(|| index.to_string(), canonical_ident);
                let variant_name = variant
                    .map(|v| format!("{}::", v.ident))
                    .unwrap_or_default();
                omission_docs.push(format!(
                    "Omitted `{variant_name}{field_name}`: {}",
                    reason.value()
                ));
                continue;
            }
            if opts.into.is_some()
                && let Some(span) = options.redact
            {
                combine(
                    &mut errors,
                    syn::Error::new(
                        span,
                        "snapshot `redact` cannot control an existing destination; implement Debug on the destination",
                    ),
                );
            }
            let mut field_type = field.ty.clone();
            if let Some(rename) = &mut rename {
                rename.visit_type_mut(&mut field_type);
                rename.options(&mut options);
            }
            let source = source_type(&field_type, &source);
            let value = if let Some(value) = &options.value {
                source_type(value, &syn::parse_quote!(#name #source_arguments))
            } else {
                let mut checker = TypeCheck {
                    source: Some(&canonical_name),
                    source_shadowed,
                    error: None,
                };
                checker.visit_type_mut(&mut field.ty.clone());
                if let Some(error) = checker.error {
                    combine(&mut errors, error);
                    continue;
                }
                // Concrete fields are checked at their projection/call spans.
                // A redundant concrete where-bound makes rustc report only the
                // derive invocation instead of the unsupported field.
                if !input.generics.params.is_empty() {
                    generics
                        .make_where_clause()
                        .predicates
                        .push(syn::parse_quote_spanned!(field.ty.span()=> #source: #rca::Snapshot));
                }
                syn::parse_quote_spanned!(field.ty.span()=> <#source as #rca::Snapshot>::Value)
            };
            let parameter = fresh_binding(input, &format!("__RcaSnapshotField{count}"));
            let binding = fresh_binding(input, &format!("__rca_snapshot_field{count}"));
            count += 1;
            let member = field
                .ident
                .clone()
                .map_or_else(|| Member::Unnamed(index.into()), Member::Named);
            captured.push(CapturedField {
                field,
                member,
                binding,
                parameter,
                options,
                value,
                source,
            });
        }
        shapes.push(Shape {
            variant,
            fields,
            captured,
        });
    }
    if let Some(errors) = errors {
        return Err(errors);
    }
    let output = if let Some(output) = opts.name {
        output
    } else {
        let mut output = syn::parse_str::<Ident>(&format!("{}Snapshot", canonical_ident(name)))
            .map_err(|_parse_error| {
                syn::Error::new_spanned(name, "Snapshot cannot derive a valid companion name")
            })?;
        output.set_span(name.span());
        output
    };
    let all_fields: Vec<_> = shapes.iter().flat_map(|s| &s.captured).collect();
    let parameters: Vec<_> = all_fields.iter().map(|f| &f.parameter).collect();
    let values = all_fields.iter().map(|f| &f.value);
    let mut output_generics = Generics::default();
    for parameter in &parameters {
        output_generics.params.push(syn::parse_quote!(#parameter));
    }
    let output_type = if let Some(destination) = &opts.into {
        quote!(#destination)
    } else {
        quote!(#output #output_generics)
    };
    let associated_type = if opts.into.is_some() {
        output_type.clone()
    } else if parameters.is_empty() {
        quote!(#output)
    } else {
        quote!(#output<#(#values),*>)
    };
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let capture = capture_body(&shapes, opts.into.as_ref(), &output, &rca);
    let shielded = all_fields.iter().map(|field| &field.binding);
    let implementation = quote! {
        const _: () = {
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{#(drop as #shielded),*};
            impl #impl_generics #rca::Snapshot for #name #source_arguments #where_clause {
                type Value = #associated_type;
                #(#[doc = #omission_docs])*
                fn snapshot(&self) -> <Self as #rca::Snapshot>::Value {
                    #capture
                }
            }
        };
    };
    if opts.into.is_some() {
        return Ok(implementation);
    }
    let vis = &input.vis;
    let docs = copied_attributes(&input.attrs);
    let declaration = if matches!(&input.data, Data::Struct(_)) {
        let fields = declared_fields(&shapes[0]);
        let semi = if matches!(shapes[0].fields, Fields::Named(_)) {
            quote!()
        } else {
            quote!(;)
        };
        quote!(#vis struct #output #output_generics #fields #semi)
    } else {
        let variants = shapes.iter().map(|shape| {
            let variant = shape.variant.unwrap();
            let name = &variant.ident;
            let docs = copied_attributes(&variant.attrs);
            let fields = declared_fields(shape);
            quote!(#(#docs)* #name #fields)
        });
        quote!(#vis enum #output #output_generics { #(#variants),* })
    };
    let debug = debug_implementation(input, &shapes, &output, &output_generics);
    Ok(quote! {
        #(#docs)*
        #(#[doc = #omission_docs])*
        #[derive(::core::clone::Clone, ::core::cmp::PartialEq, ::core::cmp::Eq)]
        #declaration
        #debug
        #implementation
    })
}

/// Declares retained fields with original visibility and independent value parameters.
///
/// Tuple positions compact after omission. Named fields retain their source names.
fn declared_fields(shape: &Shape<'_>) -> TokenStream {
    let fields = shape.captured.iter().map(|f| {
        let docs = copied_attributes(&f.field.attrs);
        let vis = &f.field.vis;
        let ty = &f.parameter;
        if matches!(shape.fields, Fields::Named(_)) {
            let member = &f.member;
            quote!(#(#docs)* #vis #member: #ty)
        } else {
            quote!(#(#docs)* #vis #ty)
        }
    });
    match shape.fields {
        Fields::Named(_) => quote!({ #(#fields),* }),
        Fields::Unnamed(_) => quote!(( #(#fields),* )),
        Fields::Unit => quote!(),
    }
}

/// Borrows retained fields from a source value or its compact companion.
///
/// `captured_only` selects companion tuple positions for debug formatting.
/// Source tuples instead keep wildcard placeholders at omitted positions.
fn pattern(shape: &Shape<'_>, root: TokenStream, captured_only: bool) -> TokenStream {
    match shape.fields {
        Fields::Named(_) => {
            let fields = shape.captured.iter().map(|f| {
                let member = &f.member;
                let binding = &f.binding;
                quote!(#member: ref #binding)
            });
            quote!(#root { #(#fields,)* .. })
        }
        Fields::Unnamed(fields) => {
            let fields: Vec<_> = if captured_only {
                shape
                    .captured
                    .iter()
                    .map(|f| {
                        let binding = &f.binding;
                        quote!(ref #binding)
                    })
                    .collect()
            } else {
                let mut captured = shape.captured.iter().peekable();
                (0..fields.unnamed.len())
                    .map(|i| {
                        captured
                            .next_if(|f| f.member == Member::Unnamed(i.into()))
                            .map_or_else(
                                || quote!(_),
                                |f| {
                                    let binding = &f.binding;
                                    quote!(ref #binding)
                                },
                            )
                    })
                    .collect()
            };
            quote!(#root ( #(#fields),* ))
        }
        Fields::Unit => root,
    }
}

/// Captures each retained field once in source order and constructs its owned output.
///
/// Enum arms preserve case identity. Existing destinations use expression-compatible
/// generic paths. Rust construction handles partial-value cleanup if capture panics.
fn capture_body(
    shapes: &[Shape<'_>],
    destination: Option<&TypePath>,
    output: &Ident,
    rca: &TokenStream,
) -> TokenStream {
    let arms = shapes.iter().map(|shape| {
        let variant = shape.variant.map(|v| &v.ident);
        let constructor = if let Some(destination) = destination {
            let mut path = destination.path.clone();
            for segment in &mut path.segments {
                if let syn::PathArguments::AngleBracketed(arguments) = &mut segment.arguments {
                    arguments.colon2_token = Some(syn::token::PathSep::default());
                }
            }
            quote!(#path)
        } else if let Some(variant) = variant {
            quote!(#output::#variant)
        } else {
            quote!(#output)
        };
        let fields = shape.captured.iter().map(|f| {
            let member = &f.member;
            let binding = &f.binding;
            let receiver = if variant.is_some() {
                quote!(#binding)
            } else {
                quote_spanned!(f.field.ty.span()=> &self.#member)
            };
            let value = &f.value;
            let expression = if let Some(function) = &f.options.with {
                quote_spanned!(f.field.span()=> ::core::convert::identity::<#value>(#function(#receiver)))
            } else {
                let ty = &f.source;
                quote_spanned!(f.field.ty.span()=> <#ty as #rca::Snapshot>::snapshot(#receiver))
            };
            if matches!(shape.fields, Fields::Named(_)) {
                quote!(#member: #expression)
            } else {
                expression
            }
        });
        let construct = match shape.fields {
            Fields::Named(_) => quote!(#constructor { #(#fields),* }),
            Fields::Unnamed(_) => quote!(#constructor ( #(#fields),* )),
            Fields::Unit => constructor,
        };
        if let Some(variant) = variant {
            let pat = pattern(shape, quote!(Self::#variant), false);
            quote!(#pat => #construct)
        } else {
            construct
        }
    });
    if shapes.first().is_some_and(|s| s.variant.is_none()) {
        quote!(#(#arms)*)
    } else {
        quote!(match *self { #(#arms),* })
    }
}

/// Formats companion values without exposing redacted data or requiring its `Debug` implementation.
///
/// Visible fields alone receive debug bounds. Every formatter failure propagates
/// unchanged, and redaction affects neither capture nor equality.
fn debug_implementation(
    input: &DeriveInput,
    shapes: &[Shape<'_>],
    output: &Ident,
    generics: &Generics,
) -> TokenStream {
    let formatter = fresh_binding(input, "__rca_snapshot_formatter");
    let builder = fresh_binding(input, "__rca_snapshot_debug");
    let mut bounded = generics.clone();
    for field in shapes
        .iter()
        .flat_map(|s| &s.captured)
        .filter(|f| f.options.redact.is_none())
    {
        let parameter = &field.parameter;
        bounded
            .make_where_clause()
            .predicates
            .push(syn::parse_quote!(#parameter: ::core::fmt::Debug));
    }
    let (impl_generics, ty_generics, where_clause) = bounded.split_for_impl();
    let shielded = shapes
        .iter()
        .flat_map(|shape| &shape.captured)
        .map(|field| &field.binding)
        .chain([&formatter, &builder]);
    let arms = shapes.iter().map(|shape| {
        let variant = shape.variant.map(|v| &v.ident);
        let root = variant.map_or_else(|| quote!(Self), |v| quote!(Self::#v));
        let pat = pattern(shape, root, true);
        let label = variant.map_or_else(
            || canonical_ident(output),
            |v| format!("{}::{}", canonical_ident(output), canonical_ident(v)),
        );
        let fields = shape.captured.iter().map(|f| {
            let binding = &f.binding;
            let value = if f.options.redact.is_some() {
                quote!(&"<redacted>")
            } else {
                quote!(#binding)
            };
            if matches!(shape.fields, Fields::Named(_)) {
                let label = f.field.ident.as_ref().map(canonical_ident).unwrap();
                quote!(#builder.field(#label, #value);)
            } else {
                quote!(#builder.field(#value);)
            }
        });
        let render = match shape.fields {
            Fields::Named(_) => quote! {
                let mut #builder = #formatter.debug_struct(#label);
                #(#fields)*
                #builder.finish()
            },
            Fields::Unnamed(_) => quote! {
                let mut #builder = #formatter.debug_tuple(#label);
                #(#fields)*
                #builder.finish()
            },
            Fields::Unit => quote!(#formatter.write_str(#label)),
        };
        quote!(#pat => { #render })
    });
    quote! {
        const _: () = {
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{#(drop as #shielded),*};
            impl #impl_generics ::core::fmt::Debug for #output #ty_generics #where_clause {
                fn fmt(&self, #formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    match *self { #(#arms),* }
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use quote::{ToTokens, quote};
    use syn::visit_mut::VisitMut;

    #[test]
    fn copied_attributes_preserve_raw_documentation_and_configuration_only() {
        let input: syn::DeriveInput = syn::parse_quote! {
            #[r#doc = "source documentation"]
            #[r#cfg(all())]
            #[repr(C)]
            #[snapshot(name = History)]
            struct Source;
        };
        let copied = super::copied_attributes(&input.attrs);
        assert_eq!(copied.len(), 2);
        assert_eq!(
            copied[0].to_token_stream().to_string(),
            input.attrs[0].to_token_stream().to_string(),
        );
        assert_eq!(
            copied[1].to_token_stream().to_string(),
            input.attrs[1].to_token_stream().to_string(),
        );
    }

    #[test]
    fn tuple_capture_keeps_sparse_source_positions_and_compacts_only_history() {
        let input = syn::parse_quote! {
            enum Source {
                Tuple(
                    #[snapshot(omit = "first omitted")] usize,
                    String,
                    #[snapshot(omit = "middle omitted")] usize,
                    String,
                    #[snapshot(omit = "last omitted")] usize,
                ),
            }
        };
        let expanded = super::expand(&input).unwrap().to_string();
        assert!(expanded.contains(
            "Self :: Tuple (_ , ref __rca_snapshot_field0 , _ , ref __rca_snapshot_field1 , _)"
        ));
        assert!(
            expanded
                .contains("Self :: Tuple (ref __rca_snapshot_field0 , ref __rca_snapshot_field1)")
        );
    }

    fn snapshot_impl(syntax: &syn::File) -> &syn::ItemImpl {
        let Some(syn::Item::Const(item)) = syntax.items.last() else {
            panic!("expected the snapshot implementation block");
        };
        let syn::Expr::Block(block) = item.expr.as_ref() else {
            panic!("expected an implementation block expression");
        };
        block
            .block
            .stmts
            .iter()
            .find_map(|statement| match statement {
                syn::Stmt::Item(syn::Item::Impl(item)) => Some(item),
                _ => None,
            })
            .expect("snapshot implementation exists")
    }

    #[test]
    fn direct_recursion_still_requires_finite_capture_with_shadowing() {
        for input in [
            syn::parse_quote!(
                struct Source {
                    next: Box<Source>,
                }
            ),
            syn::parse_quote!(
                struct Source<Source> {
                    next: Box<Self>,
                }
            ),
            syn::parse_quote!(
                enum Event<Event> {
                    Next(Box<Self>),
                    Value(Event),
                }
            ),
        ] {
            assert_eq!(
                super::expand(&input).unwrap_err().to_string(),
                "recursive snapshot capture requires a finite custom capture or manual implementation"
            );
        }
    }

    #[test]
    fn qualified_payload_with_same_name_is_not_syntactic_recursion() {
        let input = syn::parse_quote! {
            struct Source { nested: other::Source }
        };
        let expanded = super::expand(&input).unwrap();
        let syntax: syn::File = syn::parse2(expanded).unwrap();
        let syn::Item::Struct(companion) = &syntax.items[0] else {
            panic!("expected companion")
        };
        assert_eq!(companion.fields.len(), 1);
    }

    #[test]
    fn companion_uses_captured_types_not_source_generics() {
        let input = syn::parse_quote! {
            pub struct Source<'a, T, const N: usize> {
                value: T,
                #[snapshot(with = own, value = String)]
                label: &'a str,
                bytes: [u8; N],
            }
        };
        let expanded = super::expand(&input).unwrap();
        let syntax: syn::File = syn::parse2(expanded.clone()).unwrap();
        let syn::Item::Struct(companion) = &syntax.items[0] else {
            panic!("expected companion")
        };
        assert_eq!(companion.generics.type_params().count(), 3);
        assert_eq!(companion.generics.lifetimes().count(), 0);
        assert_eq!(companion.generics.const_params().count(), 0);
        insta::assert_snapshot!(expanded.to_string());
    }

    #[test]
    fn enum_capture_omits_authority_and_retains_redacted_equality() {
        let input = syn::parse_quote! {
            #[repr(u8)]
            #[snapshot(name = History)]
            pub enum Source<T> {
                Idle = 2,
                Busy {
                    #[doc = "Private captured value"]
                    value: T,
                    #[snapshot(omit = "Authority is separately asserted")]
                    task: Handle,
                    #[snapshot(with = secret, value = Secret, redact)]
                    credential: Credential,
                },
                Pair(#[snapshot(omit = "Not historical state")] Handle, T),
            }
        };
        let expanded = super::expand(&input).unwrap();
        let syntax: syn::File = syn::parse2(expanded.clone()).unwrap();
        let syn::Item::Enum(companion) = &syntax.items[0] else {
            panic!("expected enum companion")
        };
        assert_eq!(companion.generics.type_params().count(), 3);
        assert!(
            companion
                .variants
                .iter()
                .all(|variant| variant.discriminant.is_none())
        );
        insta::assert_snapshot!(expanded.to_string());
    }

    #[test]
    fn into_emits_only_the_owned_destination_implementation() {
        let input = syn::parse_quote! {
            #[snapshot(into = History<String>)]
            struct Source<'a> {
                #[snapshot(with = own, value = String)]
                value: &'a str,
                #[snapshot(omit = "Not historical state")]
                task: Handle,
            }
        };
        let expanded = super::expand(&input).unwrap();
        let syntax: syn::File = syn::parse2(expanded.clone()).unwrap();
        assert_eq!(syntax.items.len(), 1);
        assert!(matches!(syntax.items[0], syn::Item::Const(_)));
        insta::assert_snapshot!(expanded.to_string());
    }

    #[test]
    fn independent_invalid_fields_are_reported_together() {
        let input = syn::parse_quote! {
            struct Source {
                #[snapshot(omit = " ")] first: usize,
                #[snapshot(with = capture)] second: usize,
            }
        };
        let errors = super::expand(&input)
            .unwrap_err()
            .into_iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            errors,
            [
                "snapshot omission requires a non-empty reason",
                "snapshot `with` and `value` must be supplied together",
            ]
        );
    }

    #[test]
    fn invalid_snapshot_options_report_their_own_diagnostics() {
        for (source, expected) in [
            ("#[snapshot(name)] struct Source;", "expected `=`"),
            ("#[snapshot(into)] struct Source;", "expected `=`"),
            (
                "#[snapshot(into = \"History\")] struct Source;",
                "expected identifier",
            ),
            (
                "struct Source { #[snapshot(with)] value: usize }",
                "expected `=`",
            ),
            (
                "struct Source { #[snapshot(value)] value: usize }",
                "expected `=`",
            ),
            (
                "struct Source { #[snapshot(omit = 7)] value: usize }",
                "expected string literal",
            ),
            (
                "#[snapshot(name = One, name = Two)] struct Source { value: usize }",
                "duplicate snapshot option `name`",
            ),
            (
                "#[snapshot(into = <Source as Trait>::Output)] struct Source { value: usize }",
                "snapshot `into` requires a named struct path, not a qualified associated type",
            ),
            (
                "#[snapshot(name = One, into = Output)] struct Source { value: usize }",
                "snapshot `name` and `into` are mutually exclusive",
            ),
            (
                "struct Source { #[snapshot(omit = \"reason\", with = capture, value = usize)] value: usize }",
                "snapshot `omit` cannot combine with capture or redaction",
            ),
            (
                "struct Source { #[snapshot(omit = \"reason\", redact)] value: usize }",
                "snapshot `omit` cannot combine with capture or redaction",
            ),
            (
                "struct Source { #[snapshot(with = capture, value = &'static str)] value: usize }",
                "snapshot custom values must be owned; use an owned value or a manual implementation",
            ),
            (
                "#[snapshot(into = Output)] struct Source(usize);",
                "snapshot `into` supports only named-field source structs; use a manual implementation",
            ),
            (
                "enum Source { #[snapshot(omit = \"case\")] Value }",
                "snapshot options are not supported on variants; case identity is always captured",
            ),
            (
                "#[snapshot(into = Output)] struct Source { #[snapshot(redact)] value: usize }",
                "snapshot `redact` cannot control an existing destination; implement Debug on the destination",
            ),
        ] {
            let input: syn::DeriveInput = syn::parse_str(source).expect("valid Rust input");
            let errors = super::expand(&input)
                .expect_err("invalid snapshot input")
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            assert_eq!(errors, [expected], "{source}");
        }
    }

    #[test]
    fn shadowed_generic_preserves_type_value_and_absolute_path_namespaces() {
        let input = syn::parse_quote! {
            #[snapshot(into = History<<Projected as Trait>::Value>)]
            struct Projected<Projected: Trait, Other> {
                inherited: <Projected as Trait>::Value,
                absolute: ::core::marker::PhantomData<Other>,
                #[snapshot(with = Projected::capture, value = Projected::Value)]
                selected: Projected,
                #[snapshot(with = bare, value = Other)]
                fallback: Other,
            }
        };
        let syntax: syn::File = syn::parse2(super::expand(&input).expect("valid expansion"))
            .expect("parseable expansion");
        let implementation = snapshot_impl(&syntax);
        let expected_source: syn::Type = syn::parse_quote!(Projected<__RcaSnapshotSource, Other>);
        assert_eq!(
            implementation.self_ty.to_token_stream().to_string(),
            expected_source.to_token_stream().to_string(),
        );
        assert_eq!(
            implementation
                .generics
                .type_params()
                .map(|parameter| parameter.ident.to_string())
                .collect::<Vec<_>>(),
            ["__RcaSnapshotSource", "Other"],
        );
        let associated = implementation
            .items
            .iter()
            .find_map(|item| match item {
                syn::ImplItem::Type(item) => Some(item),
                _ => None,
            })
            .expect("captured value type exists");
        let expected_value: syn::Type =
            syn::parse_quote!(History<<__RcaSnapshotSource as Trait>::Value>);
        assert_eq!(
            associated.ty.to_token_stream().to_string(),
            expected_value.to_token_stream().to_string(),
        );

        let mut rename = super::ParameterRename {
            from: "Projected",
            to: syn::parse_quote!(__RcaSnapshotSource),
        };
        let mut options = super::Options {
            into: Some(syn::parse_quote!(History<<Projected as Trait>::Value>)),
            with: Some(syn::parse_quote!(Projected::capture)),
            value: Some(syn::parse_quote!(::core::marker::PhantomData<Projected>)),
            ..super::Options::default()
        };
        rename.options(&mut options);
        let expected_destination: syn::TypePath =
            syn::parse_quote!(History<<__RcaSnapshotSource as Trait>::Value>);
        assert_eq!(
            options.into.unwrap().to_token_stream().to_string(),
            expected_destination.to_token_stream().to_string(),
        );
        let expected_value: syn::Type =
            syn::parse_quote!(::core::marker::PhantomData<__RcaSnapshotSource>);
        assert_eq!(
            options.value.unwrap().to_token_stream().to_string(),
            expected_value.to_token_stream().to_string(),
        );
        let expected_function: syn::Path = syn::parse_quote!(__RcaSnapshotSource::capture);
        assert_eq!(
            options.with.unwrap().to_token_stream().to_string(),
            expected_function.to_token_stream().to_string(),
        );
        let mut bare = super::Options {
            with: Some(syn::parse_quote!(Projected)),
            ..super::Options::default()
        };
        rename.options(&mut bare);
        assert_eq!(
            bare.with.unwrap().to_token_stream().to_string(),
            "Projected"
        );

        let mut array: syn::Type = syn::parse_quote!([Other; Projected]);
        rename.visit_type_mut(&mut array);
        assert_eq!(
            array.to_token_stream().to_string(),
            quote!([Other; Projected]).to_string()
        );
    }

    #[test]
    fn omitted_only_source_has_plain_companion_value() {
        let input = syn::parse_quote! {
            struct Source {
                #[snapshot(omit = "Transient state is not historical")]
                transient: usize,
            }
        };
        let syntax: syn::File = syn::parse2(super::expand(&input).expect("valid expansion"))
            .expect("parseable expansion");
        let syn::Item::Struct(companion) = &syntax.items[0] else {
            panic!("expected a companion struct");
        };
        assert_eq!(companion.ident, "SourceSnapshot");
        assert!(companion.generics.params.is_empty());
        assert!(companion.fields.is_empty());
        let associated = snapshot_impl(&syntax)
            .items
            .iter()
            .find_map(|item| match item {
                syn::ImplItem::Type(item) => Some(item),
                _ => None,
            })
            .expect("captured value type exists");
        let expected: syn::Type = syn::parse_quote!(SourceSnapshot);
        assert_eq!(
            associated.ty.to_token_stream().to_string(),
            expected.to_token_stream().to_string(),
        );
    }

    #[test]
    fn plain_into_destination_constructs_named_fields_without_turbofish() {
        let input = syn::parse_quote! {
            #[snapshot(into = History)]
            struct Source { value: usize }
        };
        let syntax: syn::File = syn::parse2(super::expand(&input).expect("valid expansion"))
            .expect("parseable expansion");
        let implementation = snapshot_impl(&syntax);
        let snapshot = implementation
            .items
            .iter()
            .find_map(|item| match item {
                syn::ImplItem::Fn(item) => Some(item),
                _ => None,
            })
            .expect("snapshot constructor exists");
        let [syn::Stmt::Expr(syn::Expr::Struct(capture), None)] = snapshot.block.stmts.as_slice()
        else {
            panic!("expected a named destination construction");
        };
        assert!(capture.path.is_ident("History"));
        assert!(matches!(
            capture.path.segments[0].arguments,
            syn::PathArguments::None
        ));
        assert_eq!(capture.fields.len(), 1);
        assert_eq!(
            capture.fields[0].member.to_token_stream().to_string(),
            "value"
        );
    }
}
