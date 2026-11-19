//! Generates explicit test values for caller-owned dependency clients.
//!
//! Rustc supplies enabled fields before this derive runs. Endpoint checks inspect
//! syntax, not resolved types; ordinary member expressions remain caller-owned.
//! The generated constructor uses only standard-library paths and adds no live fallback.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    Data, DeriveInput, Expr, Field, Fields, GenericArgument, Path, PathArguments, PathSegment,
    Type, TypeParamBound,
};

/// Generates one inherent constructor without changing the client's fields or bounds.
///
/// # Errors
///
/// Rejects unsupported item shapes, misplaced or ambiguous options, and endpoint
/// storage outside the `Arc<dyn Fn(...) + Send + Sync>` contract.
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    if let Some(attribute) = input
        .attrs
        .iter()
        .find(|attr| crate::path_is_ident(attr.path(), "dependency"))
    {
        return Err(syn::Error::new_spanned(
            attribute,
            "dependency client options belong on fields",
        ));
    }
    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            fields => {
                return Err(syn::Error::new_spanned(
                    fields,
                    "DependencyClient requires a named-field struct",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                input,
                "DependencyClient requires a named-field struct",
            ));
        }
    };
    let location = syn::Ident::new("__rca_dependency_location", proc_macro2::Span::mixed_site());
    let mut values = Vec::with_capacity(fields.len());
    for field in fields {
        let name = field
            .ident
            .as_ref()
            .expect("Syn's named-field parser supplies an identifier");
        let (expression, member) = field_expression(field)?;
        let count = if member { 0 } else { endpoint_arity(field)? };
        let value = if let Some(expression) = expression {
            quote!(#expression)
        } else {
            let arguments = (0..count).map(|_| quote!(_));
            let label = crate::canonical_ident(name);
            // A local function shields the binding from caller constants: mixed-site
            // hygiene alone does not stop a `let` pattern resolving to a constant.
            // Keep this scope outside all caller-provided initialiser expressions.
            quote! {{
                #[track_caller]
                fn #location() -> &'static ::std::panic::Location<'static> {
                    ::std::panic::Location::caller()
                }
                let #location = #location();
                ::std::sync::Arc::new(move |#(#arguments),*| {
                    ::std::panic!(
                        "unconfigured test dependency endpoint {}.{} (test configuration at {})",
                        ::std::any::type_name::<Self>(),
                        #label,
                        #location,
                    )
                })
            }}
        };
        values.push(quote!(#name: #value));
    }
    let (ident, alias) = crate::type_reference(
        input,
        &input.generics,
        &input.generics,
        &input.ident,
        "DependencyClient",
    );
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    let implementation = quote! {
        impl #impl_generics #ident #type_generics #where_clause {
            /// Constructs explicit endpoint test defaults without using production values.
            ///
            /// Unannotated endpoints panic when called, before any returned future is polled.
            /// Their diagnostics include this construction call site, not the invocation site.
            /// Override individual endpoints with ordinary struct update syntax.
            /// Explicit initialisers run in field order.
            ///
            /// # Panics
            ///
            /// Panics if an explicit endpoint or member initialiser panics.
            #[track_caller]
            pub fn test_value() -> Self {
                Self { #(#values),* }
            }
        }
    };
    if alias.is_empty() {
        Ok(implementation)
    } else {
        Ok(quote!(const _: () = { #alias #implementation };))
    }
}

/// Parses a field's explicit value and whether it bypasses endpoint validation.
///
/// An absent attribute selects an unconfigured endpoint. A member always has an
/// expression. Duplicate attributes are rejected before their option contents.
fn field_expression(field: &Field) -> syn::Result<(Option<Expr>, bool)> {
    let mut attribute = None;
    for candidate in &field.attrs {
        if crate::path_is_ident(candidate.path(), "dependency")
            && attribute.replace(candidate).is_some()
        {
            return Err(syn::Error::new_spanned(
                candidate,
                "duplicate `dependency` attribute",
            ));
        }
    }
    let Some(attribute) = attribute else {
        return Ok((None, false));
    };
    let mut test = None;
    let mut member = false;
    attribute.parse_nested_meta(|meta| {
        let is_member = crate::path_is_ident(&meta.path, "member");
        if !is_member && !crate::path_is_ident(&meta.path, "test") {
            return Err(meta.error("unknown dependency client option; expected `test` or `member`"));
        }
        if test.is_some() {
            return Err(meta.error(if member == is_member {
                if member {
                    "duplicate `member` option"
                } else {
                    "duplicate `test` option"
                }
            } else {
                "`test` and `member` options are mutually exclusive"
            }));
        }
        member = is_member;
        test = Some(meta.value()?.parse()?);
        Ok(())
    })?;
    test.map(|test| (Some(test), member)).ok_or_else(|| {
        syn::Error::new_spanned(
            attribute,
            "missing `test` or `member` dependency client option",
        )
    })
}

/// Validates endpoint storage and type syntax, then counts its call arguments.
///
/// Explicit endpoint defaults use the same grammar as unconfigured endpoints.
/// Rustc checks arguments, results, lifetimes and expressions. The generated
/// closure only needs the argument count; it does not retain those arguments.
fn endpoint_arity(field: &Field) -> syn::Result<usize> {
    let storage_error = || {
        syn::Error::new_spanned(
            field,
            "dependency client endpoints require `Arc<dyn Fn(...) -> Return + Send + Sync>`; whole-field aliases are unsupported",
        )
    };
    let Type::Path(storage) = &field.ty else {
        return Err(storage_error());
    };
    if storage.qself.is_some() {
        return Err(storage_error());
    }
    let Some(segment) = path_segment(&storage.path, "Arc", &["std"], "sync") else {
        return Err(storage_error());
    };
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(storage_error());
    };
    if arguments.args.len() != 1 {
        return Err(storage_error());
    }
    let Some(GenericArgument::Type(Type::TraitObject(endpoint))) = arguments.args.first() else {
        return Err(storage_error());
    };
    if endpoint.dyn_token.is_none() {
        return Err(storage_error());
    }

    let mut arity = None;
    let mut send = false;
    let mut sync = false;
    for bound in &endpoint.bounds {
        match bound {
            TypeParamBound::Lifetime(_) => {}
            TypeParamBound::Trait(bound) if bound.maybe.is_none() => {
                if let Some(segment) = standard_trait(&bound.path, "Fn", "ops") {
                    let PathArguments::Parenthesized(arguments) = &segment.arguments else {
                        return Err(storage_error());
                    };
                    if arity.replace(arguments.inputs.len()).is_some() {
                        return Err(storage_error());
                    }
                } else if let Some(segment) = standard_trait(&bound.path, "Send", "marker")
                    && matches!(segment.arguments, PathArguments::None)
                {
                    send = true;
                } else if let Some(segment) = standard_trait(&bound.path, "Sync", "marker")
                    && matches!(segment.arguments, PathArguments::None)
                {
                    sync = true;
                } else {
                    return Err(storage_error());
                }
            }
            _ => return Err(storage_error()),
        }
    }
    match arity {
        Some(arity) if send && sync => Ok(arity),
        _ => Err(storage_error()),
    }
}

/// Recognises the bare, `std` and `core` spellings of a standard trait.
fn standard_trait<'a>(path: &'a Path, name: &str, module: &str) -> Option<&'a PathSegment> {
    path_segment(path, name, &["std", "core"], module)
}

/// Matches one or three path segments without resolving aliases or generic arguments.
///
/// The caller checks arguments on the final segment. Rustc rejects invalid
/// intermediate arguments; this helper does not act as a second type checker.
fn path_segment<'a>(
    path: &'a Path,
    name: &str,
    roots: &[&str],
    module: &str,
) -> Option<&'a PathSegment> {
    let mut segments = path.segments.iter();
    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (Some(segment), None, None, None) if ident_is(&segment.ident, name) => Some(segment),
        (Some(first), Some(second), Some(segment), None)
            if roots.iter().any(|root| ident_is(&first.ident, root))
                && ident_is(&second.ident, module)
                && ident_is(&segment.ident, name) =>
        {
            Some(segment)
        }
        _ => None,
    }
}

/// Matches ordinary names directly and ignores raw-identifier prefixes.
fn ident_is(ident: &syn::Ident, name: &str) -> bool {
    ident == name || ident.to_string().strip_prefix("r#") == Some(name)
}

#[cfg(test)]
mod expansion_tests {
    #[test]
    fn mixed_generic_raw_client_preserves_explicit_values_and_endpoint_arity() {
        let input = syn::parse_quote! {
            pub struct Client<T> where T: 'static {
                #[dependency(member = ::std::marker::PhantomData)]
                marker: ::std::marker::PhantomData<T>,
                pub r#type: ::std::sync::Arc<dyn Fn(T, usize) -> T + Send + Sync>,
                #[dependency(test = ::std::sync::Arc::new(|| __rca_dependency_location))]
                explicit: ::std::sync::Arc<dyn Fn() -> usize + Send + Sync>,
            }
        };
        insta::assert_snapshot!(super::expand(&input).unwrap().to_string());
    }

    #[test]
    fn empty_named_client_constructs_unit_domain() {
        let input = syn::parse_quote!(
            struct Empty {}
        );
        insta::assert_snapshot!(super::expand(&input).unwrap().to_string());
    }

    fn rejects(source: &str, diagnostic: &str) {
        let input = syn::parse_str(source).expect("valid derive input");
        let error = super::expand(&input).expect_err("invalid dependency client");
        assert_eq!(error.to_string(), diagnostic, "{source}");
    }

    #[test]
    fn rejects_non_named_structs_and_item_level_options() {
        for source in [
            "enum Client { Endpoint }",
            "union Client { value: usize }",
            "struct Client(usize);",
            "struct Client;",
        ] {
            rejects(source, "DependencyClient requires a named-field struct");
        }
        rejects(
            "#[dependency(test = ())] struct Client {}",
            "dependency client options belong on fields",
        );
    }

    #[test]
    fn raw_helpers_preserve_configuration_and_validation() {
        let input = syn::parse_quote! {
            struct Client {
                #[r#dependency(r#member = 7)]
                value: usize,
                #[dependency(r#test = Arc::new(|| 3))]
                call: Arc<dyn Fn() -> usize + Send + Sync>,
            }
        };
        super::expand(&input).expect("raw identifiers name the same helpers");
        for (source, diagnostic) in [
            (
                "#[r#dependency(test = ())] struct Client {}",
                "dependency client options belong on fields",
            ),
            (
                "struct Client { #[dependency(test = first())] #[r#dependency(test = second())] call: Arc<dyn Fn() + Send + Sync> }",
                "duplicate `dependency` attribute",
            ),
            (
                "struct Client { #[dependency(test = first(), r#test = second())] call: Arc<dyn Fn() + Send + Sync> }",
                "duplicate `test` option",
            ),
        ] {
            rejects(source, diagnostic);
        }
    }

    #[test]
    fn raw_standard_paths_preserve_owned_endpoint_arity() {
        use syn::parse::Parser;

        for ty in [
            "r#Arc<dyn r#Fn(String, usize) -> String + r#Send + r#Sync>",
            "::r#std::r#sync::r#Arc<dyn ::r#core::r#ops::r#Fn(String, usize) -> String + ::r#std::r#marker::r#Send + ::r#core::r#marker::r#Sync>",
        ] {
            let field = syn::Field::parse_named
                .parse_str(&format!("call: {ty}"))
                .expect("valid Rust endpoint syntax");
            assert_eq!(super::endpoint_arity(&field).unwrap(), 2, "{ty}");
        }
    }

    #[test]
    fn constant_expressions_do_not_make_endpoint_arguments_borrowed() {
        let input = syn::parse_quote! {
            struct Client {
                call: Arc<dyn Fn([u8; {
                    let length: &usize = &3;
                    *length
                }]) -> usize + Send + Sync>,
            }
        };
        super::expand(&input).expect("the endpoint owns the computed-size array");
    }

    #[test]
    fn function_pointer_binders_do_not_borrow_endpoint_payloads() {
        use quote::ToTokens;

        let input: syn::DeriveInput = syn::parse_quote! {
            struct Client {
                callback: Arc<dyn Fn(for<'a> fn(&'a str) -> &'a str) + Send + Sync>,
                external: Arc<dyn Fn(unsafe extern "C" fn(*const u8) -> usize) + Send + Sync>,
                returned: Arc<dyn Fn() -> for<'a> fn(&'a str) -> &'a str + Send + Sync>,
            }
        };
        let source = input.to_token_stream().to_string();
        super::expand(&input).expect("function pointers do not capture a borrowed environment");
        assert_eq!(input.to_token_stream().to_string(), source);
    }

    #[test]
    fn rejects_ambiguous_or_missing_field_configuration() {
        for (attributes, diagnostic) in [
            ("#[dependency(test)]", "expected `=`"),
            ("#[dependency(member)]", "expected `=`"),
            ("#[dependency(test = ;)]", "expected an expression"),
            ("#[dependency(member = ;)]", "expected an expression"),
            (
                "#[dependency(test = first())] #[dependency(test = second())]",
                "duplicate `dependency` attribute",
            ),
            (
                "#[dependency(test = first(), test = second())]",
                "duplicate `test` option",
            ),
            (
                "#[dependency(member = first(), member = second())]",
                "duplicate `member` option",
            ),
            (
                "#[dependency(test = first(), member = second())]",
                "`test` and `member` options are mutually exclusive",
            ),
            (
                "#[dependency(member = first(), test = second())]",
                "`test` and `member` options are mutually exclusive",
            ),
            (
                "#[dependency(live = first())]",
                "unknown dependency client option; expected `test` or `member`",
            ),
            (
                "#[dependency()]",
                "missing `test` or `member` dependency client option",
            ),
        ] {
            rejects(
                &format!("struct Client {{ {attributes} call: Arc<dyn Fn() + Send + Sync> }}"),
                diagnostic,
            );
        }
    }

    #[test]
    fn rejects_non_arc_storage_and_non_fn_endpoints() {
        let diagnostic = "dependency client endpoints require `Arc<dyn Fn(...) -> Return + Send + Sync>`; whole-field aliases are unsupported";
        for ty in [
            "&'static str",
            "Endpoint",
            "<Provider as Contract>::Arc<dyn Fn() + Send + Sync>",
            "Box<dyn Fn() + Send + Sync>",
            "alloc::sync::Arc<dyn Fn() + Send + Sync>",
            "sync::Arc<dyn Fn() + Send + Sync>",
            "std::sync::Arc<dyn Fn() + Send + Sync>::Extra",
            "std::sync::Arc<dyn Fn() + Send + Sync>::Extra::More",
            "core::sync::Arc<dyn Fn() + Send + Sync>",
            "std::ops::Arc<dyn Fn() + Send + Sync>",
            "std::sync::Rc<dyn Fn() + Send + Sync>",
            "Arc",
            "Arc<>",
            "Arc<dyn Fn() + Send + Sync, Allocator>",
            "Arc<usize>",
            "Arc<1>",
            "Arc<Send + Sync>",
            "Arc<dyn Fn + Send + Sync>",
            "Arc<dyn Fn() + Fn(String) + Send + Sync>",
            "Arc<dyn FnMut() + Send + Sync>",
            "Arc<dyn FnOnce() + Send + Sync>",
            "Arc<dyn ops::Fn() + Send + Sync>",
            "Arc<dyn extra::std::ops::Fn() + Send + Sync>",
            "Arc<dyn extra::more::std::ops::Fn() + Send + Sync>",
            "Arc<dyn alloc::ops::Fn() + Send + Sync>",
            "Arc<dyn std::marker::Fn() + Send + Sync>",
            "Arc<dyn std::ops::FnMut() + Send + Sync>",
            "Arc<dyn Fn() + marker::Send + Sync>",
            "Arc<dyn Fn() + std::marker::Send::Extra + Sync>",
            "Arc<dyn Fn() + alloc::marker::Send + Sync>",
            "Arc<dyn Fn() + Send + std::ops::Sync>",
            "Arc<dyn Fn() + Send() + Sync>",
            "Arc<dyn Fn() + Send + Sync()>",
            "Arc<dyn Fn<()> + Send + Sync>",
            "Arc<dyn Fn() + Clone + Send + Sync>",
            "Arc<dyn Fn() + Send<()> + Sync>",
            "Arc<dyn Fn() + Send + Sync<()>>",
            "Arc<dyn Send + Sync>",
            "Arc<dyn Fn() + Send>",
            "Arc<dyn Fn() + Sync>",
            "Arc<dyn Fn() + ?Send + Sync>",
        ] {
            rejects(&format!("struct Client {{ call: {ty} }}"), diagnostic);
        }
        rejects(
            "struct Client { #[dependency(test = 1)] value: usize }",
            diagnostic,
        );
    }

    #[test]
    fn borrowed_and_higher_ranked_endpoint_signatures_preserve_arity() {
        use syn::parse::Parser;

        for (ty, expected) in [
            ("Arc<dyn Fn(&str) + Send + Sync>", 1),
            ("Arc<dyn Fn(Vec<&'static str>) + Send + Sync>", 1),
            ("Arc<dyn Fn() -> &'static str + Send + Sync>", 0),
            ("Arc<dyn Fn(Cow<'a, str>) + Send + Sync>", 1),
            (
                "Arc<dyn Fn(Vec<&'static for<'a> fn(&'a str)>) + Send + Sync>",
                1,
            ),
            ("Arc<dyn for<'a> Fn(&'a str) -> &'a str + Send + Sync>", 1),
            ("Arc<dyn Fn() + Send + Sync + 'a>", 0),
        ] {
            let field = syn::Field::parse_named
                .parse_str(&format!("call: {ty}"))
                .expect("valid field syntax");
            assert_eq!(super::endpoint_arity(&field).unwrap(), expected, "{ty}");
        }
    }

    #[test]
    fn recognised_endpoint_spellings_preserve_arity() {
        use syn::parse::Parser;

        // These are parsed syntax contracts, not claims that every spelling
        // resolves as a Rust type (notably arguments on intermediate segments).
        for (ty, expected) in [
            ("Arc<dyn Fn() + Send + Sync>", 0),
            ("::Arc<dyn ::Fn(u8) + ::Send + ::Sync>", 1),
            (
                "std::sync::Arc<dyn std::ops::Fn(u8, u16) + std::marker::Send + std::marker::Sync>",
                2,
            ),
            (
                "::std::sync::Arc<dyn ::core::ops::Fn(u8, u16, u32) + ::core::marker::Send + ::core::marker::Sync>",
                3,
            ),
            ("Arc<dyn Sync + Send + Fn(u8, u16)>", 2),
            ("std<u8>::sync<u16>::Arc<dyn Fn(u8, u16) + Send + Sync>", 2),
            (
                "Arc<dyn std<u8>::ops<u16>::Fn(u8, u16) + std<u8>::marker<u16>::Send + core<u8>::marker<u16>::Sync>",
                2,
            ),
        ] {
            let field = syn::Field::parse_named
                .parse_str(&format!("call: {ty}"))
                .expect("valid field syntax");
            assert_eq!(super::endpoint_arity(&field).unwrap(), expected, "{ty}");
        }
    }

    #[test]
    fn private_endpoint_boundary_preserves_empty_and_trailing_path_policy() {
        use syn::parse::Parser;

        // Safe constructed syn trees exercise the private boundary's existing
        // policy. Empty and trailing-punctuation paths are not public Rust syntax.
        for edit_storage in [true, false] {
            for trailing_punctuation in [false, true] {
                let mut field = syn::Field::parse_named
                    .parse_str("call: Arc<dyn Fn(u8, u16) + Send + Sync>")
                    .unwrap();
                let syn::Type::Path(storage) = &mut field.ty else {
                    panic!("expected endpoint storage");
                };
                let path = if edit_storage {
                    &mut storage.path
                } else {
                    let syn::PathArguments::AngleBracketed(arguments) =
                        &mut storage.path.segments.last_mut().unwrap().arguments
                    else {
                        panic!("expected endpoint argument");
                    };
                    let Some(syn::GenericArgument::Type(syn::Type::TraitObject(endpoint))) =
                        arguments.args.first_mut()
                    else {
                        panic!("expected endpoint trait object");
                    };
                    let Some(syn::TypeParamBound::Trait(bound)) = endpoint.bounds.first_mut()
                    else {
                        panic!("expected function trait");
                    };
                    &mut bound.path
                };
                if trailing_punctuation {
                    path.segments.push_punct(syn::token::PathSep::default());
                    assert_eq!(super::endpoint_arity(&field).unwrap(), 2);
                } else {
                    path.segments.clear();
                    assert_eq!(
                        super::endpoint_arity(&field).unwrap_err().to_string(),
                        "dependency client endpoints require `Arc<dyn Fn(...) -> Return + Send + Sync>`; whole-field aliases are unsupported",
                    );
                }
            }
        }
    }

    #[test]
    fn valid_owned_endpoints_preserve_explicit_endpoint_and_member_values() {
        use quote::ToTokens;

        for ty in [
            "Arc<dyn Fn(String) -> String + Send + Sync + 'static>",
            "std::sync::Arc<dyn std::ops::Fn(String) -> String + std::marker::Send + std::marker::Sync>",
            "::std::sync::Arc<dyn ::core::ops::Fn(Cow<'static, str>) -> usize + ::core::marker::Send + ::core::marker::Sync>",
            "Arc<dyn Sync + Send + Fn(fn())>",
        ] {
            let input = syn::parse_str(&format!(
                "#[doc = \"client\"]
                 struct Client {{
                     #[doc = \"endpoint before\"]
                     #[dependency(test = make_endpoint())]
                     #[doc = \"endpoint after\"]
                     call: {ty},
                     #[dependency(member = make_context())]
                     context: std::rc::Rc<String>,
                 }}"
            ))
            .expect("valid derive input");
            let expanded = super::expand(&input).expect("owned endpoint and explicit member");
            let implementation: syn::ItemImpl =
                syn::parse2(expanded).expect("generated implementation");
            let [syn::ImplItem::Fn(method)] = implementation.items.as_slice() else {
                panic!("expected the test-value constructor");
            };
            assert_eq!(method.sig.ident, "test_value");
            let [syn::Stmt::Expr(syn::Expr::Struct(value), None)] = method.block.stmts.as_slice()
            else {
                panic!("expected named field initialisers");
            };
            let fields: Vec<_> = value
                .fields
                .iter()
                .map(|field| {
                    (
                        field.member.to_token_stream().to_string(),
                        field.expr.to_token_stream().to_string(),
                    )
                })
                .collect();
            assert_eq!(
                fields,
                [
                    ("call".to_owned(), "make_endpoint ()".to_owned()),
                    ("context".to_owned(), "make_context ()".to_owned()),
                ],
                "{ty}"
            );
        }
    }
}
