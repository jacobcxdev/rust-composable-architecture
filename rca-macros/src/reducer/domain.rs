//! Lowers associated payload placeholders and reuses the existing path expanders.
//!
//! Supplied domains retain their exact types and emit no declarations. Generated
//! domains retain caller-written visibility, generics and Rust expressions.
//! Only selected payload types interpret `_` as an associated-type placeholder.
//! Conditional members produce symbolic guards, not host-dependent decisions.

use proc_macro2::TokenStream;
use quote::quote;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, Data, DeriveInput, Fields, Ident, Type};

use super::{
    cfg,
    syntax::{self, Arguments, Case, CaseAction, Child, Domain, Selection},
};
use crate::{action_paths, canonical_ident, path_is_ident, state_paths};

/// Finds a declared child, treating raw and ordinary names as equivalent.
///
/// # Errors
///
/// Returns an error at the selected name if no declaration matches.
pub(super) fn find_child<'a>(arguments: &'a Arguments, name: &Ident) -> syn::Result<&'a Child> {
    let selected = canonical_ident(name);
    arguments
        .children
        .iter()
        .find(|child| canonical_ident(&child.name) == selected)
        .ok_or_else(|| {
            syn::Error::new_spanned(name, "child selection has no matching child declaration")
        })
}

/// Finds a declared case, treating raw and ordinary names as equivalent.
///
/// # Errors
///
/// Returns an error at the selected name if no declaration matches.
pub(super) fn find_case<'a>(arguments: &'a Arguments, name: &Ident) -> syn::Result<&'a Case> {
    let selected = canonical_ident(name);
    arguments
        .cases
        .iter()
        .find(|case| canonical_ident(&case.name) == selected)
        .ok_or_else(|| {
            syn::Error::new_spanned(name, "case selection has no matching case declaration")
        })
}

/// Returns the domain type and any generated declaration and path items.
///
/// Empty state is unit; empty action is uninhabited. Selected child payloads use
/// the reducer's associated types. Explicit case-state payloads remain unchanged.
/// Paths consume helper attributes before the declaration loses those helpers.
///
/// # Errors
///
/// Returns syntax, selection, payload-shape or path-generation errors at their
/// source locations. Members retain source order when several errors are possible.
pub(super) fn expand(
    domain: &Domain,
    arguments: &Arguments,
    is_state: bool,
    rca: &TokenStream,
    storage_generics: &syn::Generics,
) -> syn::Result<(Type, TokenStream)> {
    let item = match domain {
        Domain::Supplied(ty) => return Ok((ty.as_ref().clone(), TokenStream::new())),
        Domain::Empty => {
            return Ok((
                if is_state {
                    syn::parse_quote!(())
                } else {
                    syn::parse_quote!(::core::convert::Infallible)
                },
                TokenStream::new(),
            ));
        }
        Domain::Generated(item) => item,
    };
    let mut declaration: DeriveInput = syn::parse2(quote!(#item))?;
    match &mut declaration.data {
        Data::Struct(data) if is_state => {
            for field in &mut data.fields {
                if let Some(selection) = syntax::selection(&mut field.attrs)? {
                    replace(&mut field.ty, selection, arguments, is_state, rca)?;
                }
            }
        }
        Data::Enum(data) => {
            for variant in &mut data.variants {
                if let Some(selection) = syntax::selection(&mut variant.attrs)? {
                    if let (true, Selection::Case(name)) = (is_state, &selection) {
                        find_case(arguments, name)?;
                    } else {
                        let mut fields = variant.fields.iter_mut();
                        let (Some(payload), None) = (fields.next(), fields.next()) else {
                            return Err(syn::Error::new_spanned(
                                variant,
                                "selected action or child case requires exactly one payload",
                            ));
                        };
                        replace(&mut payload.ty, selection, arguments, is_state, rca)?;
                    }
                }
            }
        }
        _ => {
            return Err(syn::Error::new_spanned(
                &declaration,
                "generated state requires a struct or enum; generated action requires an enum",
            ));
        }
    }
    // The same domain appears in the storage impl and in free binding helpers.
    // Each helper owns its generic scope, independently of the storage type.
    let mut scopes = storage_generics.clone();
    let where_clause = &storage_generics.where_clause;
    let mut scope_tokens = quote!(#declaration #storage_generics #where_clause);
    for binding in arguments.children.iter().map(|child| &child.binding).chain(
        arguments
            .cases
            .iter()
            .filter_map(|case| case.reduction.as_ref().map(|reduction| &reduction.binding)),
    ) {
        let signature = &binding.signature;
        scope_tokens.extend(quote!(#signature));
        scopes
            .params
            .extend(binding.signature.generics.params.iter().cloned());
    }
    let (name, alias) = crate::type_reference(
        &scope_tokens,
        &scopes,
        &declaration.generics,
        &declaration.ident,
        "ReducerDomain",
    );
    let type_arguments = state_paths::type_arguments(&declaration.generics);
    let ty = syn::parse_quote!(#name #type_arguments);
    let paths = paths(&declaration, is_state)?;
    strip_helpers(&mut declaration);
    Ok((ty, quote!(#declaration #alias #paths)))
}

/// Replaces inferred payload types without entering embedded Rust expressions.
struct Placeholder<'a> {
    /// Borrowed type cloned into each selected payload placeholder.
    replacement: &'a Type,
    /// Payload placeholders replaced, excluding inference inside expressions.
    ///
    /// The caller requires exactly one.
    count: usize,
}

impl VisitMut for Placeholder<'_> {
    fn visit_type_mut(&mut self, i: &mut Type) {
        if matches!(i, Type::Infer(_)) {
            self.count += 1;
            *i = self.replacement.clone();
        } else {
            visit_mut::visit_type_mut(self, i);
        }
    }

    // Const expressions own their inference and nested items. Their `_` types
    // are not associated-type placeholders for the surrounding payload.
    fn visit_expr_mut(&mut self, _: &mut syn::Expr) {}
}

/// Lowers one selected type placeholder, preserving explicit case state.
fn replace(
    ty: &mut Type,
    selection: Selection,
    arguments: &Arguments,
    is_state: bool,
    rca: &TokenStream,
) -> syn::Result<()> {
    let replacement = match selection {
        Selection::Child(name) => {
            let reducer = &find_child(arguments, &name)?.reducer;
            if is_state {
                syn::parse_quote!(<#reducer as #rca::Reducer>::State)
            } else {
                syn::parse_quote!(<#reducer as #rca::Reducer>::Action)
            }
        }
        Selection::Case(name) => {
            let case = find_case(arguments, &name)?;
            if is_state {
                return Ok(());
            }
            match &case.action {
                CaseAction::Empty => syn::parse_quote!(::core::convert::Infallible),
                CaseAction::Supplied(action) if case.reduction.is_some() => {
                    syn::parse_quote!(#rca::PresentationAction<#action>)
                }
                CaseAction::Supplied(action) => action.as_ref().clone(),
            }
        }
        _ => {
            return Err(syn::Error::new_spanned(
                ty,
                "domain payload requires child or case selection",
            ));
        }
    };
    let original = ty.clone();
    let mut visitor = Placeholder {
        replacement: &replacement,
        count: 0,
    };
    visitor.visit_type_mut(ty);
    if visitor.count != 1 {
        return Err(syn::Error::new_spanned(
            original,
            "selected payload requires exactly one explicit `_` associated-type placeholder",
        ));
    }
    Ok(())
}

/// Expands whole domains, or gates each member under its source condition.
fn paths(declaration: &DeriveInput, is_state: bool) -> syn::Result<TokenStream> {
    let expand = |input: &DeriveInput, fallback: Option<&TokenStream>| {
        if is_state {
            state_paths::expand_parts(input, fallback, declaration)
        } else {
            action_paths::expand_parts(input, fallback, declaration)
        }
    };
    let conditional = |attributes: &[Attribute]| {
        attributes.iter().any(|attribute| {
            path_is_ident(attribute.path(), "cfg") || path_is_ident(attribute.path(), "cfg_attr")
        })
    };
    let selections: Vec<(Data, Vec<Attribute>)> = match &declaration.data {
        Data::Struct(syn::DataStruct {
            struct_token,
            fields: Fields::Named(fields),
            semi_token,
        }) if fields.named.iter().any(|field| conditional(&field.attrs)) => fields
            .named
            .iter()
            .map(|field| {
                (
                    Data::Struct(syn::DataStruct {
                        struct_token: *struct_token,
                        fields: Fields::Named(syn::FieldsNamed {
                            brace_token: fields.brace_token,
                            named: std::iter::once(field.clone()).collect(),
                        }),
                        semi_token: *semi_token,
                    }),
                    field.attrs.clone(),
                )
            })
            .collect(),
        Data::Enum(data)
            if data
                .variants
                .iter()
                .any(|variant| conditional(&variant.attrs)) =>
        {
            data.variants
                .iter()
                .map(|variant| {
                    (
                        Data::Enum(syn::DataEnum {
                            enum_token: data.enum_token,
                            brace_token: data.brace_token,
                            variants: std::iter::once(variant.clone()).collect(),
                        }),
                        variant.attrs.clone(),
                    )
                })
                .collect()
        }
        // Unconditional domains retain D05's complete collision diagnostics.
        _ => return expand(declaration, None).map(state_paths::PathExpansion::into_tokens),
    };
    // Parse ahead for other-variant reachability, but preserve each member's
    // expansion-before-condition diagnostic order below.
    let conditions: Vec<_> = selections
        .iter()
        .map(|(_, attributes)| cfg::condition(attributes))
        .collect();
    // Reuse one namespace and guard each owned top-level member item.
    // Validate every member before interpreting its condition; rustc checks
    // constructor collisions after selecting active cfgs.
    let mut namespace = None;
    let mut members = Vec::new();
    for (index, (data, _)) in selections.into_iter().enumerate() {
        let selected = DeriveInput {
            attrs: declaration.attrs.clone(),
            vis: declaration.vis.clone(),
            ident: declaration.ident.clone(),
            generics: declaration.generics.clone(),
            data,
        };
        let fallback =
            matches!(&selected.data, Data::Enum(_)).then(|| variant_fallback(&conditions, index));
        let expanded = expand(&selected, fallback.as_ref())?;
        namespace.get_or_insert(expanded.namespace);
        let condition = conditions[index].as_ref().map_err(Clone::clone)?;
        let attribute = cfg::attribute(condition.as_ref());
        members.extend(
            expanded
                .cases
                .into_iter()
                .flatten()
                .chain(std::iter::once(expanded.constructors))
                .map(|item| quote!(#attribute #item)),
        );
    }
    Ok(quote!(#namespace #(#members)*))
}

/// Adds a mismatch arm only when another enum variant can exist.
fn variant_fallback(
    conditions: &[syn::Result<Option<TokenStream>>],
    selected: usize,
) -> TokenStream {
    let mut predicates = Vec::new();
    for (index, condition) in conditions.iter().enumerate() {
        if index == selected {
            continue;
        }
        // A malformed condition is reported after its own member is validated;
        // no expansion containing that error can be returned successfully.
        match condition {
            Ok(None) => return quote!(_ => ::core::option::Option::None,),
            Ok(Some(predicate)) => predicates.push(predicate),
            Err(_) => {}
        }
    }
    if predicates.is_empty() {
        TokenStream::new()
    } else {
        quote!(#[cfg(any(#(#predicates),*))] _ => ::core::option::Option::None,)
    }
}

/// Removes consumed path helpers without rewriting nested Rust items.
fn strip_helpers(declaration: &mut DeriveInput) {
    /// Removes only `rca` attributes from this field or variant.
    fn strip(attributes: &mut Vec<Attribute>) {
        attributes.retain(|attribute| !attribute.path().is_ident("rca"));
    }
    /// Visits declaration members without entering their types or discriminants.
    struct StripHelpers;

    impl VisitMut for StripHelpers {
        fn visit_field_mut(&mut self, i: &mut syn::Field) {
            strip(&mut i.attrs);
        }

        fn visit_variant_mut(&mut self, i: &mut syn::Variant) {
            strip(&mut i.attrs);
            self.visit_fields_mut(&mut i.fields);
        }
    }

    // Do not traverse declaration metadata, payload types or discriminants.
    StripHelpers.visit_data_mut(&mut declaration.data);
}

#[cfg(test)]
mod tests {
    use super::paths;
    use crate::{action_paths, state_paths};
    use proc_macro2::TokenStream;
    use quote::{ToTokens, quote};
    use syn::DeriveInput;

    fn direct(input: &DeriveInput, is_state: bool) -> syn::Result<TokenStream> {
        if is_state {
            state_paths::expand(input)
        } else {
            action_paths::expand(input)
        }
    }

    // A skipped route still needs a mismatch, but only while its case exists.
    #[test]
    fn conditional_mismatch_fallback_tracks_other_enum_variants() {
        struct Fallbacks(Vec<String>);

        impl syn::visit_mut::VisitMut for Fallbacks {
            fn visit_arm_mut(&mut self, i: &mut syn::Arm) {
                if matches!(i.pat, syn::Pat::Wild(_)) {
                    let attributes = &i.attrs;
                    self.0.push(quote!(#(#attributes)*).to_string());
                }
                syn::visit_mut::visit_arm_mut(self, i);
            }
        }

        for (source, expected) in [
            (
                "enum Domain { #[cfg(feature = \"ready\")] Ready(u8) }",
                vec![],
            ),
            (
                "enum Domain { Ready(u8), #[cfg(feature = \"other\")] #[rca(skip)] Other }",
                vec![quote!(#[cfg(any(feature = "other"))]).to_string()],
            ),
            (
                "enum Domain { Ready, #[cfg(feature = \"other\")] #[rca(skip)] Other }",
                vec![quote!(#[cfg(any(feature = "other"))]).to_string()],
            ),
            (
                "enum Domain { Ready { first: u8, second: u16 }, #[cfg(feature = \"other\")] #[rca(skip)] Other }",
                vec![quote!(#[cfg(any(feature = "other"))]).to_string()],
            ),
            (
                "enum Domain { #[cfg(feature = \"ready\")] Ready(u8), #[rca(skip)] Other }",
                vec![String::new()],
            ),
            (
                "enum Domain { Ready(u8), #[cfg_attr(feature = \"guard\", cfg_attr(feature = \"nested\", cfg(feature = \"other\")))] #[rca(skip)] Other }",
                vec![quote!(#[cfg(any(any(not(feature = "guard"), any(not(feature = "nested"), feature = "other"))))]).to_string()],
            ),
            (
                "enum Domain { #[cfg(feature = \"side\")] Ready(u8), #[cfg(not(feature = \"side\"))] Ready(u16) }",
                vec![
                    quote!(#[cfg(any(not(feature = "side")))]).to_string(),
                    quote!(#[cfg(any(feature = "side"))]).to_string(),
                ],
            ),
        ] {
            let input: DeriveInput = syn::parse_str(source).unwrap();
            for is_state in [false, true] {
                let mut output: syn::File = syn::parse2(paths(&input, is_state).unwrap()).unwrap();
                let mut actual = Fallbacks(Vec::new());
                syn::visit_mut::VisitMut::visit_file_mut(&mut actual, &mut output);
                let expected = if is_state {
                    expected.iter().flat_map(|attribute| [attribute.clone(), attribute.clone()]).collect()
                } else {
                    expected.clone()
                };
                assert_eq!(actual.0, expected, "{source}, is_state={is_state}");
            }
        }
    }

    #[test]
    fn conditional_fallback_preserves_member_diagnostic_order() {
        for (source, expected) in [
            (
                "enum Domain { #[cfg] Crate(u8), Other }",
                "RCA derives cannot generate identifier `crate` because it is not a legal raw Rust identifier",
            ),
            (
                "enum Domain { #[cfg] Ready(u8), Crate(u8) }",
                "expected cfg(predicate)",
            ),
            (
                "enum Domain { Crate(u8), #[cfg] Other }",
                "RCA derives cannot generate identifier `crate` because it is not a legal raw Rust identifier",
            ),
            (
                "enum Domain { Ready(u8), #[cfg] #[rca(skip)] Other }",
                "expected cfg(predicate)",
            ),
        ] {
            let input: DeriveInput = syn::parse_str(source).unwrap();
            for is_state in [false, true] {
                assert_eq!(
                    paths(&input, is_state).unwrap_err().to_string(),
                    expected,
                    "{source}, is_state={is_state}",
                );
            }
        }
    }

    #[test]
    fn unconditional_paths_preserve_tokens_and_diagnostics() {
        let declarations = [
            "struct State {}",
            "struct State<'a, T, const N: usize> where T: 'a { r#type: &'a T, values: [T; N] }",
            "struct State(u64);",
            "struct State;",
            "enum State {}",
            "enum State<T> { Idle, r#type(T), Named { value: T, extra: u64 } }",
            "enum State { HTTPRequest(u64), HttpRequest(u64) }",
            "union State { value: u64 }",
        ];
        for declaration in declarations {
            let input: DeriveInput = syn::parse_str(declaration).unwrap();
            for is_state in [true, false] {
                let tokens = |result: syn::Result<TokenStream>| {
                    result
                        .unwrap_or_else(syn::Error::into_compile_error)
                        .to_string()
                };
                assert_eq!(
                    tokens(paths(&input, is_state)),
                    tokens(direct(&input, is_state)),
                    "{declaration}, is_state={is_state}"
                );
            }
        }
    }

    #[test]
    fn forbidden_case_methods_reject_across_payload_shapes_and_conditions() {
        for (case, method) in [("Crate", "crate"), ("Self_", "self"), ("Super", "super")] {
            for payload in [
                "",
                "()",
                "{}",
                "(u8)",
                "(u8, u16)",
                "{ first: u8, second: u16 }",
            ] {
                for condition in ["", "#[cfg(any())]", "#[cfg]", "#[cfg_attr]"] {
                    let source = format!("enum Domain {{ {condition} {case}{payload} }}");
                    let input: DeriveInput = syn::parse_str(&source).unwrap();
                    for is_state in [false, true] {
                        let error = paths(&input, is_state)
                            .expect_err("a reserved method cannot be emitted or raw-escaped");
                        assert_eq!(
                            error.to_string(),
                            format!(
                                "RCA derives cannot generate identifier `{method}` because it is not a legal raw Rust identifier"
                            ),
                            "{source}, is_state={is_state}",
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "checks item order and every conditional boundary of one expansion together"
    )]
    fn conditional_generic_aggregate_paths_preserve_item_boundaries() {
        fn cfg_tokens(attributes: &[syn::Attribute]) -> String {
            let conditions = attributes.iter().filter(|attribute| {
                attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr")
            });
            quote!(#(#conditions)*).to_string()
        }

        let input: DeriveInput = syn::parse_quote! {
            enum Domain<T> {
                #[cfg(feature = "selected")]
                #[cfg_attr(feature = "guarded", cfg(feature = "payload"))]
                #[doc = "source variant only"]
                Ready(T, u16),
                #[cfg(any())]
                #[rca(skip)]
                Skipped(T),
            }
        };
        let original = quote! {
            #[cfg(feature = "selected")]
            #[cfg_attr(feature = "guarded", cfg(feature = "payload"))]
        };
        let combined = quote! {
            #[cfg(all(feature = "selected", any(not(feature = "guarded"), feature = "payload")))]
        };
        let expected_alias: syn::Type =
            syn::parse_quote!(__rca_path_namespace_DomainPaths::Namespace<Domain<T>>);
        for is_state in [false, true] {
            let output: syn::File = syn::parse2(paths(&input, is_state).unwrap()).unwrap();
            let [
                syn::Item::Mod(namespace),
                syn::Item::Type(alias),
                syn::Item::Struct(marker),
                syn::Item::Const(gate),
                syn::Item::Impl(constructors),
                syn::Item::Impl(skipped),
            ] = output.items.as_slice()
            else {
                panic!("expected one generic namespace, marker, const and two constructor impls");
            };
            assert_eq!(namespace.ident, "__rca_path_namespace_DomainPaths");
            assert_eq!(alias.ident, "DomainPaths");
            assert_eq!(
                alias.generics.to_token_stream().to_string(),
                quote!(<T>).to_string()
            );
            assert_eq!(
                alias.ty.to_token_stream().to_string(),
                expected_alias.to_token_stream().to_string()
            );
            assert!(cfg_tokens(&namespace.attrs).is_empty());
            assert!(cfg_tokens(&alias.attrs).is_empty());
            for attributes in [&marker.attrs, &gate.attrs] {
                assert_eq!(
                    cfg_tokens(attributes),
                    quote!(#combined #original).to_string()
                );
            }
            assert_eq!(cfg_tokens(&constructors.attrs), combined.to_string());
            assert_eq!(
                constructors.self_ty.to_token_stream().to_string(),
                quote!(DomainPaths<T>).to_string()
            );
            let [syn::ImplItem::Fn(constructor)] = constructors.items.as_slice() else {
                panic!("expected only the selected aggregate constructor");
            };
            assert_eq!(constructor.sig.ident, "ready");
            assert_eq!(cfg_tokens(&constructor.attrs), original.to_string());
            assert_eq!(
                cfg_tokens(&skipped.attrs),
                quote!(#[cfg(any())]).to_string()
            );
            assert!(
                skipped.items.is_empty(),
                "skipped members retain an empty impl"
            );
            assert_eq!(
                skipped.self_ty.to_token_stream().to_string(),
                quote!(DomainPaths<T>).to_string()
            );
            let syn::Expr::Block(block) = gate.expr.as_ref() else {
                panic!("aggregate implementations must remain inside the gated const");
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
                    let path = &implementation.trait_.as_ref().unwrap().0;
                    path.segments.last().unwrap().ident.to_string()
                })
                .collect();
            if is_state {
                assert_eq!(traits, ["AggregateCase", "MutableAggregateCase"]);
                assert_eq!(cfg_tokens(&implementations[1].attrs), original.to_string());
            } else {
                assert_eq!(traits, ["AggregateCase"]);
            }
        }
    }

    #[test]
    fn conditional_paths_preserve_tokens_and_diagnostics() {
        let declarations = [
            "struct State { #[cfg(unix)] first: u64, second: u64 }",
            "struct State<T> { #[cfg_attr(unix, cfg_attr(target_os = \"macos\", cfg(feature = \"child\")))] r#type: T, #[cfg_attr(unix, doc = \"description\")] value: T }",
            "enum State<T> { #[cfg(unix)] HTTPRequest(T), #[cfg(not(unix))] HttpRequest(T) }",
            "enum State { #[cfg_attr(unix, cfg(any()))] First(u64), Second { value: u64, extra: u64 } }",
            "enum State { #[cfg_attr] First(u64) }",
            "enum State { #[cfg] First(u64) }",
        ];
        let mut outputs = String::new();
        for declaration in declarations {
            let input: DeriveInput = syn::parse_str(declaration).unwrap();
            for is_state in [true, false] {
                use std::fmt::Write;
                // Compare parsed syntax, not punctuation spacing from token grouping.
                let output =
                    paths(&input, is_state).map_or_else(syn::Error::into_compile_error, |tokens| {
                        syn::parse2::<syn::File>(tokens)
                            .expect("generated conditional paths must remain valid Rust syntax")
                            .into_token_stream()
                    });
                writeln!(outputs, "{declaration}\nis_state={is_state}\n{output}\n").unwrap();
            }
        }
        insta::assert_snapshot!(outputs);
    }
}

#[cfg(test)]
mod contract_tests {
    use super::*;
    use quote::ToTokens;

    fn arguments() -> Arguments {
        syn::parse_str(
            "state(empty), action(empty), body(generated),
             child(child, reducer = Child, kind = required, state = state_path,
                 action = action_path, bind(fn bind_child(child: Child))),
             case(alert, policy = data, action = supplied(AlertAction)),
             case(ignored, policy = ignored, action = empty)",
        )
        .expect("explicit declarations")
    }

    #[test]
    fn empty_domains_distinguish_unit_state_from_uninhabited_action() {
        let arguments = arguments();
        for (is_state, expected) in [(true, "()"), (false, "::core::convert::Infallible")] {
            let domain: Domain = syn::parse_str("empty").unwrap();
            let (ty, items) = expand(
                &domain,
                &arguments,
                is_state,
                &quote!(::rca),
                &syn::Generics::default(),
            )
            .unwrap();
            let expected: Type = syn::parse_str(expected).unwrap();
            assert_eq!(
                ty.to_token_stream().to_string(),
                expected.to_token_stream().to_string()
            );
            assert!(
                items.is_empty(),
                "empty domains do not declare generated items"
            );
        }
    }

    #[test]
    fn generated_domain_aliases_include_storage_parameters_and_all_members() {
        let arguments = arguments();
        for (source, is_state, storage, expected) in [
            (
                "struct State<State> { #[cfg(all())] value: State, #[cfg(all())] other: [State; __RcaStatePathsState] }",
                true,
                "<State>",
                "__RcaReducerDomainState<State>",
            ),
            (
                "enum Action<Action> { #[cfg(all())] Value(Action), #[cfg(all())] Pair(Action, [Action; __RcaActionPathsAction]) }",
                false,
                "<Action>",
                "__RcaReducerDomainAction<Action>",
            ),
            (
                "struct State<T> { value: T }",
                true,
                "<State, T>",
                "__RcaReducerDomainState<T>",
            ),
        ] {
            let domain: Domain = syn::parse_str(&format!("generated({source})")).unwrap();
            let generics = syn::parse_str(storage).unwrap();
            let (ty, expanded) =
                expand(&domain, &arguments, is_state, &quote!(::rca), &generics).unwrap();
            let expected: Type = syn::parse_str(expected).unwrap();
            assert_eq!(
                ty.to_token_stream().to_string(),
                expected.to_token_stream().to_string()
            );
            let _: syn::File = syn::parse2(expanded).unwrap();
        }
    }

    #[test]
    fn generated_domains_avoid_independent_binding_generics() {
        for selection in [
            "child(child, reducer = Child, kind = required, state = state_path,
                action = action_path,
                bind(fn bind_child<State, Action, __RcaReducerDomainState>(child: Child)))",
            "case(alert, policy = ephemeral, action = supplied(AlertAction),
                state_path = state_path, action_path = action_path,
                identity = identity, predicate = predicate,
                bind(fn bind_alert<State, Action, __RcaReducerDomainState>(parent: Parent)))",
            "child(child, reducer = Child, kind = required, state = state_path,
                action = action_path,
                bind(fn bind_child<State, Action>(child: Child)
                    where State: Into<__RcaReducerDomainState>))",
        ] {
            let arguments: Arguments = syn::parse_str(&format!(
                "state(empty), action(empty), body(generated), {selection}"
            ))
            .unwrap();
            for (source, is_state, expected) in [
                ("struct State {}", true, "__RcaReducerDomainState_"),
                ("enum Action {}", false, "__RcaReducerDomainAction"),
            ] {
                let domain: Domain = syn::parse_str(&format!("generated({source})")).unwrap();
                let (ty, expanded) = expand(
                    &domain,
                    &arguments,
                    is_state,
                    &quote!(::rca),
                    &syn::Generics::default(),
                )
                .unwrap();
                assert_eq!(ty.to_token_stream().to_string(), expected);
                let _: syn::File = syn::parse2(expanded).unwrap();
            }
        }
    }

    fn ephemeral_arguments(policy: &str) -> Arguments {
        syn::parse_str(&format!(
            "state(empty), action(empty), body(generated),
             case(alert, policy = {policy}, action = supplied(AlertAction),
                 state_path = state_path, action_path = action_path,
                 identity = identity, predicate = predicate,
                 bind(fn bind_alert(parent: Parent)))"
        ))
        .expect("raw and ordinary policy spellings are accepted")
    }

    #[test]
    fn raw_ephemeral_policy_preserves_the_presentation_action() {
        for policy in ["ephemeral", "r#ephemeral"] {
            let arguments = ephemeral_arguments(policy);
            let mut ty: Type = syn::parse_quote!(_);
            replace(
                &mut ty,
                Selection::Case(syn::parse_quote!(alert)),
                &arguments,
                false,
                &quote!(::renamed_rca),
            )
            .unwrap();
            let expected: Type = syn::parse_quote!(::renamed_rca::PresentationAction<AlertAction>);
            assert_eq!(
                ty.to_token_stream().to_string(),
                expected.to_token_stream().to_string(),
                "{policy}"
            );
        }
    }

    #[test]
    fn raw_ephemeral_policy_preserves_reducer_field_selection() {
        for policy in ["ephemeral", "r#ephemeral"] {
            let arguments = ephemeral_arguments(policy);
            let mut storage = syn::parse_quote! {
                struct Feature {
                    #[reducer(case = alert)]
                    alert: AlertReducer,
                }
            };
            let (fields, errors) = super::super::select_fields(&mut storage, &arguments)
                .expect("validated ephemeral cases can select reducer fields");
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "alert");
            assert!(errors.is_empty());
        }
    }

    #[test]
    fn payload_placeholders_preserve_const_expression_inference() {
        let arguments = arguments();
        for (source, expected) in [
            (
                "[_; { let count: _ = 2usize; count }]",
                "[<Child as ::rca::Reducer>::State; { let count: _ = 2usize; count }]",
            ),
            (
                "Wrapper<_, { let count: Option<_> = Some(2usize); count.unwrap() }>",
                "Wrapper<<Child as ::rca::Reducer>::State, { let count: Option<_> = Some(2usize); count.unwrap() }>",
            ),
        ] {
            let mut ty: Type = syn::parse_str(source).unwrap();
            replace(
                &mut ty,
                Selection::Child(syn::parse_quote!(child)),
                &arguments,
                true,
                &quote!(::rca),
            )
            .expect("only the payload type contains an associated-type placeholder");
            let expected: Type = syn::parse_str(expected).unwrap();
            assert_eq!(
                ty.to_token_stream().to_string(),
                expected.to_token_stream().to_string()
            );
        }
    }

    #[test]
    fn const_expression_inference_does_not_supply_a_payload_placeholder() {
        let arguments = arguments();
        let mut ty: Type = syn::parse_quote!(
            [u8; {
                let count: _ = 2usize;
                count
            }]
        );
        let error = replace(
            &mut ty,
            Selection::Child(syn::parse_quote!(child)),
            &arguments,
            true,
            &quote!(::rca),
        )
        .expect_err("ordinary Rust inference cannot select an associated payload type");
        assert_eq!(
            error.to_string(),
            "selected payload requires exactly one explicit `_` associated-type placeholder",
        );
    }

    #[test]
    fn generated_domains_reject_invalid_shapes_and_selected_payloads() {
        let arguments = arguments();
        for (source, is_state, diagnostic) in [
            (
                "fn state() {}",
                true,
                "expected one of: `struct`, `enum`, `union`",
            ),
            (
                "struct Action {}",
                false,
                "generated state requires a struct or enum; generated action requires an enum",
            ),
            (
                "union State { value: usize }",
                true,
                "generated state requires a struct or enum; generated action requires an enum",
            ),
            (
                "struct State { #[reducer(child = missing)] value: _ }",
                true,
                "child selection has no matching child declaration",
            ),
            (
                "struct State { #[reducer(case = missing)] value: Alert }",
                true,
                "case selection has no matching case declaration",
            ),
            (
                "enum State { #[reducer(case = missing)] Missing }",
                true,
                "case selection has no matching case declaration",
            ),
            (
                "enum Action { #[reducer(child = child)] Empty }",
                false,
                "selected action or child case requires exactly one payload",
            ),
            (
                "enum Action { #[reducer(child = child)] Pair(_, _) }",
                false,
                "selected action or child case requires exactly one payload",
            ),
            (
                "struct State { #[reducer(data)] value: usize }",
                true,
                "domain payload requires child or case selection",
            ),
            (
                "enum Action { #[reducer(reduce)] Value(_) }",
                false,
                "domain payload requires child or case selection",
            ),
            (
                "struct State { #[reducer(child = child)] value: usize }",
                true,
                "selected payload requires exactly one explicit `_` associated-type placeholder",
            ),
            (
                "struct State { #[reducer(child = child)] value: (_, _) }",
                true,
                "selected payload requires exactly one explicit `_` associated-type placeholder",
            ),
            (
                "struct State { #[reducer(unknown)] value: usize }",
                true,
                "expected child, case, reduce or data selection",
            ),
            (
                "enum Action { #[reducer(unknown)] Value(usize) }",
                false,
                "expected child, case, reduce or data selection",
            ),
            (
                "struct State { #[cfg] value: usize }",
                true,
                "expected cfg(predicate)",
            ),
            (
                "struct State { #[cfg(unix)] #[rca(unknown)] value: usize }",
                true,
                "unknown rca option; expected `required`, `optional`, `keyed`, or `skip`",
            ),
        ] {
            let domain: Domain =
                syn::parse_str(&format!("generated({source})")).expect("parseable generated item");
            let error = expand(
                &domain,
                &arguments,
                is_state,
                &quote!(::rca),
                &syn::Generics::default(),
            )
            .err()
            .expect("invalid generated domain");
            assert_eq!(error.to_string(), diagnostic, "{source}");
        }
    }

    #[test]
    fn helper_stripping_preserves_non_helper_syntax_and_nested_declarations() {
        for (source, expected) in [
            (
                r#"#[doc = "root"]
                   pub struct State<'a, T: Copy> where T: 'a {
                       #[doc = "field"] #[rca(skip)] pub value: &'a T,
                   }"#,
                r#"#[doc = "root"]
                   pub struct State<'a, T: Copy> where T: 'a {
                       #[doc = "field"] pub value: &'a T,
                   }"#,
            ),
            (
                r#"#[repr(u8)]
                   enum Action<T> where T: Copy {
                       #[doc = "tuple"] #[rca(skip)]
                       Tuple(#[doc = "payload"] [T; {
                           #[derive(rca::ActionPaths)]
                           enum Nested { #[rca(skip)] Unit }
                           1
                       }]),
                       #[rca(skip)] Named { #[doc = "payload"] value: T },
                       #[rca(skip)] Unit = {
                           #[derive(rca::ActionPaths)]
                           enum Nested { #[rca(skip)] Unit }
                           7
                       },
                   }"#,
                r#"#[repr(u8)]
                   enum Action<T> where T: Copy {
                       #[doc = "tuple"]
                       Tuple(#[doc = "payload"] [T; {
                           #[derive(rca::ActionPaths)]
                           enum Nested { #[rca(skip)] Unit }
                           1
                       }]),
                       Named { #[doc = "payload"] value: T },
                       Unit = {
                           #[derive(rca::ActionPaths)]
                           enum Nested { #[rca(skip)] Unit }
                           7
                       },
                   }"#,
            ),
        ] {
            let mut declaration: DeriveInput = syn::parse_str(source).unwrap();
            let expected: DeriveInput = syn::parse_str(expected).unwrap();

            strip_helpers(&mut declaration);

            assert_eq!(
                declaration.to_token_stream().to_string(),
                expected.to_token_stream().to_string(),
                "{source}",
            );
        }
    }

    #[test]
    fn generated_declarations_lower_payloads_and_remove_helper_attributes() {
        let arguments = arguments();
        for (source, is_state, expected) in [
            (
                "struct State { #[reducer(child = child)] #[rca(optional)] child: Option<_>, #[reducer(case = alert)] alert: Alert }",
                true,
                "struct State { child: Option<<Child as ::rca::Reducer>::State>, alert: Alert }",
            ),
            (
                "enum State { #[reducer(child = child)] Child(_), #[reducer(case = alert)] #[rca(skip)] Alert }",
                true,
                "enum State { Child(<Child as ::rca::Reducer>::State), Alert }",
            ),
            (
                "enum Action { #[reducer(child = child)] Child(_), #[reducer(case = alert)] Alert(_), #[reducer(case = ignored)] Ignored(_), Refresh }",
                false,
                "enum Action { Child(<Child as ::rca::Reducer>::Action), Alert(AlertAction), Ignored(::core::convert::Infallible), Refresh }",
            ),
        ] {
            let domain: Domain = syn::parse_str(&format!("generated({source})")).unwrap();
            let (ty, expanded) = expand(
                &domain,
                &arguments,
                is_state,
                &quote!(::rca),
                &syn::Generics::default(),
            )
            .unwrap();
            let expanded: syn::File = syn::parse2(expanded).expect("valid generated Rust");
            let expected: syn::Item = syn::parse_str(expected).unwrap();
            assert_eq!(
                expanded.items[0].to_token_stream().to_string(),
                expected.to_token_stream().to_string(),
                "{source}"
            );
            assert_eq!(
                ty.to_token_stream().to_string(),
                if is_state { "State" } else { "Action" }
            );
            assert!(
                expanded.items.len() > 1,
                "generated domains include their path declarations"
            );
        }
    }
}
