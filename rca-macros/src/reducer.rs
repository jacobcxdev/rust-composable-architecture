//! Expands explicit domains and persistent, statically selected reducer composition.
//!
//! Validation follows declaration order: arguments, storage, state, action, field
//! selections, then helpers. Conditional fields and their bounds reach the same
//! staging macro, so disabled types never appear in the emitted implementation.

mod binding;
mod cfg;
mod domain;
mod syntax;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Fields, Ident, ItemStruct, Type};

use crate::{canonical_ident, rca_path, state_paths};
use syntax::{Arguments, Body, Selection};

/// Expands one reducer declaration without constructing or cloning its stored reducers.
///
/// # Errors
///
/// Returns an error for invalid declarations, selections, helpers or crate-path lookup.
pub(crate) fn expand(attribute: TokenStream, input: TokenStream) -> syn::Result<TokenStream> {
    let arguments = syn::parse2::<Arguments>(attribute)?;
    let mut storage = syn::parse2::<ItemStruct>(input)?;
    let rca = rca_path()?;
    let (state, state_items) =
        domain::expand(&arguments.state, &arguments, true, &rca, &storage.generics)?;
    let (action, action_items) = domain::expand(
        &arguments.action,
        &arguments,
        false,
        &rca,
        &storage.generics,
    )?;
    let (fields, selection_errors) = select_fields(&mut storage, &arguments)?;
    let implementation = implementation(&storage, &arguments.body, &fields, &state, &action, &rca);
    let helpers = arguments
        .children
        .iter()
        .map(|child| binding::child(child, &state, &action, &rca))
        .chain(
            arguments
                .cases
                .iter()
                .map(|case| binding::case(case, &state, &action, &rca)),
        )
        .collect::<syn::Result<Vec<_>>>()?;
    Ok(quote! {
        #storage
        #state_items
        #action_items
        #(#helpers)*
        #selection_errors
        #implementation
    })
}

/// Carries one selected storage field and the condition that also gates its bound.
struct SelectedField {
    /// Preserves the caller's field spelling and diagnostic span.
    name: Ident,
    /// Supplies the reducer bound without resolving the user's type during expansion.
    ty: Type,
    /// Uses no condition for unconditional fields; predicates remain compiler-evaluated.
    condition: Option<TokenStream>,
}

/// Removes selection markers and validates persistent storage in declaration order.
///
/// The returned fields preserve reduction order. Conditional child-storage errors
/// are emitted as guarded diagnostics, leaving predicate evaluation to the compiler.
fn select_fields(
    storage: &mut ItemStruct,
    arguments: &Arguments,
) -> syn::Result<(Vec<SelectedField>, TokenStream)> {
    let Fields::Named(fields) = &mut storage.fields else {
        return Err(syn::Error::new_spanned(
            &storage.fields,
            "reducer storage requires a named-field struct",
        ));
    };
    let mut selected = Vec::new();
    let mut child_fields = Vec::new();
    for field in &mut fields.named {
        let name = field.ident.as_ref().expect("named storage field");
        let marker = syntax::selection(&mut field.attrs)?;
        let mut child_name = None;
        let reduce = match &arguments.body {
            Body::Supplied(body) => {
                if marker.is_some() && !matches!(marker, Some(Selection::Data)) {
                    return Err(syn::Error::new_spanned(
                        name,
                        "supplied body cannot also select automatic child reduction",
                    ));
                }
                canonical_ident(name) == canonical_ident(body)
            }
            Body::Generated => match marker {
                Some(Selection::Child(child)) => {
                    domain::find_child(arguments, &child)?;
                    child_name = Some(canonical_ident(&child));
                    true
                }
                Some(Selection::Case(case)) => {
                    let case = domain::find_case(arguments, &case)?;
                    if case.reduction.is_none() {
                        return Err(syn::Error::new_spanned(
                            name,
                            "ignored/data policy cannot select a reducer field",
                        ));
                    }
                    true
                }
                Some(Selection::Reduce) => true,
                Some(Selection::Data) => false,
                None => {
                    return Err(syn::Error::new_spanned(
                        name,
                        "generated body requires an explicit child, case, reduce or data selection",
                    ));
                }
            },
        };
        if reduce {
            let condition = cfg::condition(&field.attrs)?;
            if let Some(child_name) = child_name {
                child_fields.push((child_name, condition.clone()));
            }
            selected.push(SelectedField {
                name: name.clone(),
                ty: field.ty.clone(),
                condition,
            });
        }
    }
    if let Body::Supplied(name) = &arguments.body
        && selected.is_empty()
    {
        return Err(syn::Error::new_spanned(
            name,
            "supplied body field does not exist",
        ));
    }
    let errors = if matches!(arguments.body, Body::Generated) {
        child_storage_errors(&arguments.children, &child_fields)?
    } else {
        TokenStream::new()
    };
    Ok((selected, errors))
}

/// Emits diagnostics for configurations where declared children lack selected storage.
///
/// An unconditional missing child fails immediately. Other predicates remain
/// symbolic, including distinct predicates whose equivalence is decided by rustc.
fn child_storage_errors(
    children: &[syntax::Child],
    fields: &[(String, Option<TokenStream>)],
) -> syn::Result<TokenStream> {
    let mut errors = TokenStream::new();
    for child in children {
        let name = canonical_ident(&child.name);
        let conditions: Vec<_> = fields
            .iter()
            .filter(|(selected, _)| *selected == name)
            .map(|(_, condition)| condition.as_ref())
            .collect();
        let declared = cfg::condition(&child.attributes)?;
        if conditions.iter().any(|condition| {
            condition.is_none()
                || condition.map(TokenStream::to_string)
                    == declared.as_ref().map(TokenStream::to_string)
        }) {
            continue;
        }
        let error = syn::Error::new_spanned(
            &child.name,
            "generated child declaration requires selected persistent storage",
        );
        if conditions.is_empty() && declared.is_none() {
            return Err(error);
        }
        let declared = declared.unwrap_or_else(|| quote!(all()));
        let stored = conditions.into_iter().flatten();
        let diagnostic = error.to_compile_error();
        errors.extend(quote! {
            #[cfg(all(#declared, not(any(#(#stored),*))))]
            #diagnostic
        });
    }
    Ok(errors)
}

/// Selects predicates and fields together before emitting one ordinary impl.
///
/// Each stage adds both the active field and its bound, or neither. The terminal
/// stage always exists, including an empty generated body. It emits the caller's
/// generics and existing bounds before the selected reducer bounds.
fn implementation(
    storage: &ItemStruct,
    body: &Body,
    fields: &[SelectedField],
    state: &Type,
    action: &Type,
    rca: &TokenStream,
) -> TokenStream {
    let state_value = state_paths::fresh_binding(storage, "__rca_state");
    let action_value = state_paths::fresh_binding(storage, "__rca_action");
    let context = state_paths::fresh_binding(storage, "__rca_context");
    let first = state_paths::fresh_binding(storage, "__rca_reducer_stage_0");
    let mut current = first.clone();
    let mut definitions = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        let next =
            state_paths::fresh_binding(storage, &format!("__rca_reducer_stage_{}", index + 1));
        let name = &field.name;
        let ty = &field.ty;
        let enabled = cfg::attribute(field.condition.as_ref());
        definitions.push(quote! {
            #enabled
            macro_rules! #current {
                ([$($bounds:tt)*] [$($fields:ident,)*]) => {
                    #next!([$($bounds)* #ty: #rca::Reducer<State = #state, Action = #action>,] [$($fields,)* #name,]);
                };
            }
        });
        if let Some(condition) = &field.condition {
            definitions.push(quote! {
                #[cfg(not(#condition))]
                macro_rules! #current {
                    ([$($bounds:tt)*] [$($fields:ident,)*]) => {
                        #next!([$($bounds)*] [$($fields,)*]);
                    };
                }
            });
        }
        current = next;
    }
    let terminal = current;
    let (name, alias) = crate::type_reference(
        storage,
        &storage.generics,
        &storage.generics,
        &storage.ident,
        "Reducer",
    );
    let (impl_generics, type_generics, _) = storage.generics.split_for_impl();
    let mut predicates = storage
        .generics
        .where_clause
        .as_ref()
        .map(|clause| clause.predicates.clone())
        .unwrap_or_default();
    if !predicates.is_empty() && !predicates.trailing_punct() {
        predicates.push_punct(syn::token::Comma::default());
    }
    let reduction = match body {
        Body::Supplied(_) => {
            quote!($(#rca::Reducer::reduce(&mut self.$fields, #state_value, #action_value, #context))*)
        }
        Body::Generated => {
            quote!(#rca::Effect::merge([$(#rca::Reducer::reduce(&mut self.$fields, #state_value, #action_value, #context)),*]))
        }
    };
    // The anonymous const keeps staging names private and independent across expansions.
    quote! {
        const _: () = {
            #alias
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{drop as #state_value, drop as #action_value, drop as #context};
            #(#definitions)*
            macro_rules! #terminal {
                ([$($bounds:tt)*] [$($fields:ident,)*]) => {
                    #[automatically_derived]
                    impl #impl_generics #rca::Reducer for #name #type_generics
                    where #predicates $($bounds)*
                    {
                        type State = #state;
                        type Action = #action;
                        fn reduce(&mut self, #state_value: &mut Self::State, #action_value: &Self::Action, #context: &mut #rca::ReduceContext<'_>) -> #rca::Effect<Self::Action> {
                            let _ = (&#state_value, &#action_value, &#context);
                            #reduction
                        }
                    }
                };
            }
            #first!([] []);
        };
    }
}

#[cfg(test)]
mod tests {
    use super::expand;
    use quote::quote;

    #[test]
    fn supplied_body_expansion_matches_snapshot() {
        let expanded = expand(
            quote!(
                state(supplied(State)),
                action(supplied(Action)),
                body(supplied(body))
            ),
            quote!(
                pub struct Feature<R> {
                    pub body: R,
                }
            ),
        )
        .expect("explicit supplied body expands");
        insta::assert_snapshot!(expanded.to_string());
    }

    #[test]
    fn conditional_associated_child_expansion_matches_snapshot() {
        let expanded = expand(
            quote!(
                state(generated(pub struct State {
                    #[cfg(feature = "selected")]
                    #[rca(optional)]
                    #[reducer(child = child)] pub child: Option<_>,
                })),
                action(generated(pub enum Action {
                    #[cfg(feature = "selected")]
                    #[reducer(child = child)] Child(_),
                })),
                body(generated),
                #[cfg(feature = "selected")]
                child(child, reducer = Child, kind = optional,
                    state = StatePaths::child(), action = ActionPaths::child(),
                    bind(pub fn bind_child(parent: Parent, child: Child))),
            ),
            quote!(
                pub struct Feature<C> {
                    #[cfg(feature = "selected")]
                    #[reducer(child = child)]
                    pub child: C,
                    #[reducer(data)]
                    pub marker: ::core::marker::PhantomData<C>,
                }
            ),
        )
        .expect("conditional associated child expands");
        insta::assert_snapshot!(expanded.to_string());
    }

    #[test]
    fn data_only_ephemeral_expansion_matches_snapshot() {
        let expanded = expand(
            quote!(
                state(generated(pub struct State {
                    #[reducer(case = alert)] pub alert: Option<Alert>,
                })),
                action(generated(pub enum Action {
                    #[reducer(case = alert)] Alert(_),
                    #[reducer(case = ignored)] Ignored(_),
                })),
                body(supplied(body)),
                case(alert, policy = ephemeral, action = supplied(AlertAction),
                    state_path = StatePaths::alert(), action_path = ActionPaths::alert(),
                    identity = |alert: &Alert| &alert.id,
                    predicate = |_: &Alert, _: &AlertAction| true,
                    bind(pub fn bind_alert(parent: Parent))),
                case(ignored, policy = ignored, action = empty),
            ),
            quote!(
                pub struct Feature<R> {
                    pub body: R,
                }
            ),
        )
        .expect("data-only policies expand without reducer metadata");
        insta::assert_snapshot!(expanded.to_string());
    }

    #[test]
    fn storage_shape_and_selection_errors_are_reported() {
        for (body, storage, expected) in [
            (
                quote!(generated),
                quote!(
                    struct Feature;
                ),
                "reducer storage requires a named-field struct",
            ),
            (
                quote!(generated),
                quote!(
                    struct Feature<R>(R);
                ),
                "reducer storage requires a named-field struct",
            ),
            (
                quote!(generated),
                quote!(
                    struct Feature<R> {
                        body: R,
                    }
                ),
                "generated body requires an explicit child, case, reduce or data selection",
            ),
            (
                quote!(generated),
                quote!(
                    struct Feature<R> {
                        #[reducer(child = missing)]
                        body: R,
                    }
                ),
                "child selection has no matching child declaration",
            ),
            (
                quote!(generated),
                quote!(
                    struct Feature<R> {
                        #[reducer(case = missing)]
                        body: R,
                    }
                ),
                "case selection has no matching case declaration",
            ),
            (
                quote!(supplied(body)),
                quote!(
                    struct Feature<R> {
                        other: R,
                    }
                ),
                "supplied body field does not exist",
            ),
        ] {
            let error = expand(
                quote!(
                    state(supplied(State)),
                    action(supplied(Action)),
                    body(#body)
                ),
                storage,
            )
            .expect_err("invalid storage is rejected");
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn data_only_cases_cannot_select_reducer_storage() {
        for policy in [quote!(ignored), quote!(data)] {
            let error = expand(
                quote!(
                    state(supplied(State)),
                    action(supplied(Action)),
                    body(generated),
                    case(item, policy = #policy, action = empty)
                ),
                quote!(
                    struct Feature<R> {
                        #[reducer(case = item)]
                        body: R,
                    }
                ),
            )
            .expect_err("data-only policy cannot reduce");
            assert_eq!(
                error.to_string(),
                "ignored/data policy cannot select a reducer field",
            );
        }
    }

    #[test]
    fn generated_body_selects_ephemeral_reducer_storage() {
        let expanded = expand(
            quote!(
                state(supplied(State)),
                action(supplied(Action)),
                body(generated),
                case(alert, policy = ephemeral, action = supplied(AlertAction),
                    state_path = StatePaths::alert(), action_path = ActionPaths::alert(),
                    identity = |alert: &Alert| &alert.id,
                    predicate = |_: &Alert, _: &AlertAction| true,
                    bind(pub fn bind_alert(parent: Parent))),
            ),
            quote!(
                pub struct Feature<R> {
                    #[reducer(case = alert)]
                    pub body: R,
                }
            ),
        )
        .expect("ephemeral storage participates in generated reduction")
        .to_string();
        assert!(
            expanded.contains(
                &quote!(
                    [$($bounds)* R: ::rca::Reducer<State = State, Action = Action>,]
                    [$($fields,)* body,]
                )
                .to_string()
            )
        );
        assert!(
            expanded.contains(
                &quote!(
                    pub struct Feature<R> {
                        pub body: R,
                    }
                )
                .to_string()
            )
        );
    }

    #[test]
    fn storage_where_predicates_precede_generated_reducer_bounds() {
        for storage in [
            "struct Feature<R> where R: Clone { body: R }",
            "struct Feature<R> where R: Clone, { body: R }",
        ] {
            let expanded = expand(
                quote!(
                    state(supplied(State)),
                    action(supplied(Action)),
                    body(supplied(body))
                ),
                storage.parse().expect("parseable storage tokens"),
            )
            .expect("storage predicates coexist with generated bounds")
            .to_string();
            assert!(
                expanded.contains(
                    &quote!(
                        impl<R> ::rca::Reducer for Feature<R>
                        where R: Clone, $($bounds)*
                    )
                    .to_string()
                )
            );
        }
    }

    #[test]
    fn reducer_conditions_reject_empty_and_malformed_nested_cfg_attr() {
        for (condition, expected) in [
            (
                "#[cfg_attr()]",
                "cfg_attr requires a predicate and attributes",
            ),
            ("#[cfg_attr(unix, cfg)]", "expected cfg(predicate)"),
        ] {
            let condition: proc_macro2::TokenStream =
                condition.parse().expect("parseable attribute tokens");
            for result in [
                expand(
                    quote!(
                        state(supplied(State)),
                        action(supplied(Action)),
                        body(generated)
                    ),
                    quote!(struct Feature<R> {
                        #condition
                        #[reducer(reduce)]
                        body: R,
                    }),
                ),
                expand(
                    quote!(
                        state(supplied(State)), action(supplied(Action)), body(generated),
                        #condition
                        child(child, reducer = Child, kind = required,
                            state = state_path, action = action_path,
                            bind(pub fn bind_child(child: Child)))
                    ),
                    quote!(
                        struct Feature<R> {
                            #[reducer(child = child)]
                            child: R,
                        }
                    ),
                ),
            ] {
                assert_eq!(
                    result
                        .expect_err("malformed condition is rejected")
                        .to_string(),
                    expected,
                );
            }
        }
    }

    #[test]
    fn missing_persistent_child_storage_reports_only_its_uncovered_configuration() {
        use quote::ToTokens;

        let expand_with = |declaration: proc_macro2::TokenStream,
                           fields: proc_macro2::TokenStream| {
            expand(
                quote!(
                    state(supplied(State)), action(supplied(Action)), body(generated),
                    #declaration
                    child(child, reducer = Child, kind = required,
                        state = state_path, action = action_path,
                        bind(pub fn bind_child(child: Child)))
                ),
                quote!(struct Feature { #fields }),
            )
        };
        let message = "generated child declaration requires selected persistent storage";
        assert_eq!(
            expand_with(quote!(), quote!()).unwrap_err().to_string(),
            message,
        );
        for (declaration, fields, expected) in [
            (
                quote!(),
                quote!(#[reducer(child = child)] body: Child,),
                None,
            ),
            (
                quote!(#[cfg(unix)]),
                quote!(),
                Some(quote!(cfg(all(unix, not(any()))))),
            ),
            (
                quote!(),
                quote!(#[cfg(unix)] #[reducer(child = child)] body: Child,),
                Some(quote!(cfg(all(all(), not(any(unix)))))),
            ),
            (
                quote!(#[cfg(unix)]),
                quote!(#[cfg(windows)] #[reducer(child = child)] body: Child,),
                Some(quote!(cfg(all(unix, not(any(windows)))))),
            ),
            (
                quote!(#[cfg(unix)]),
                quote!(#[cfg(unix)] #[reducer(child = child)] body: Child,),
                None,
            ),
        ] {
            let expanded =
                expand_with(declaration, fields).expect("conditional diagnostic expands");
            let file: syn::File = syn::parse2(expanded).expect("valid generated Rust items");
            let diagnostics: Vec<_> = file
                .items
                .iter()
                .filter_map(|item| match item {
                    syn::Item::Macro(item)
                        if item.mac.path.segments.last().unwrap().ident == "compile_error" =>
                    {
                        Some(item)
                    }
                    _ => None,
                })
                .collect();
            if let Some(expected) = expected {
                let [diagnostic] = diagnostics.as_slice() else {
                    panic!("expected exactly one conditional storage diagnostic");
                };
                let [attribute] = diagnostic.attrs.as_slice() else {
                    panic!("storage diagnostic must have exactly one condition");
                };
                assert_eq!(
                    attribute.meta.to_token_stream().to_string(),
                    expected.to_string()
                );
                let text: syn::LitStr = syn::parse2(diagnostic.mac.tokens.clone()).unwrap();
                assert_eq!(text.value(), message);
            } else {
                assert!(
                    diagnostics.is_empty(),
                    "matching storage satisfies the declaration"
                );
            }
        }
    }

    #[test]
    fn expansion_validates_arguments_before_storage_syntax() {
        let storage = quote!(
            enum Feature {}
        );
        let error = expand(quote!(action(empty), body(generated)), storage.clone())
            .expect_err("invalid arguments must not be hidden by the annotated item");
        assert_eq!(
            error.to_string(),
            "unexpected end of input, missing explicit state selection"
        );
        let expected = syn::parse2::<syn::ItemStruct>(storage.clone())
            .err()
            .expect("an enum is not reducer storage");
        let error = expand(
            quote!(state(empty), action(empty), body(generated)),
            storage,
        )
        .expect_err("the annotated item must be a struct");
        assert_eq!(error.to_string(), expected.to_string());
    }

    #[test]
    fn expansion_propagates_domain_errors_in_declaration_order() {
        let invalid_state = quote!(generated(struct State {
            #[reducer(child = missing)]
            child: _,
        }));
        let invalid_action = quote!(generated(enum Action {
            #[reducer(case = missing)]
            Alert(_),
        }));
        for (state, action, diagnostic) in [
            (
                invalid_state.clone(),
                quote!(supplied(Action)),
                "child selection has no matching child declaration",
            ),
            (
                quote!(supplied(State)),
                invalid_action.clone(),
                "case selection has no matching case declaration",
            ),
            (
                invalid_state,
                invalid_action,
                "child selection has no matching child declaration",
            ),
        ] {
            let error = expand(
                quote!(state(#state), action(#action), body(supplied(body))),
                quote!(
                    struct Feature<R> {
                        body: R,
                    }
                ),
            )
            .expect_err("invalid generated domains must abort the whole expansion");
            assert_eq!(error.to_string(), diagnostic);
        }
    }

    #[test]
    fn supplied_body_owns_reduction_and_still_validates_field_selections() {
        let arguments = quote!(
            state(supplied(State)),
            action(supplied(Action)),
            body(supplied(body))
        );
        for (marker, diagnostic) in [
            (
                quote!(#[reducer(reduce)]),
                "supplied body cannot also select automatic child reduction",
            ),
            (
                quote!(#[reducer(child = missing)]),
                "supplied body cannot also select automatic child reduction",
            ),
            (
                quote!(#[reducer(case = missing)]),
                "supplied body cannot also select automatic child reduction",
            ),
            (
                quote!(#[reducer(data, reduce)]),
                "conflicting or duplicate reducer selection",
            ),
        ] {
            for storage in [
                quote!(struct Feature<R> { #marker body: R, metadata: usize }),
                quote!(struct Feature<R> { body: R, #marker metadata: usize }),
            ] {
                let error = expand(arguments.clone(), storage)
                    .expect_err("supplied reduction cannot silently ignore field selections");
                assert_eq!(error.to_string(), diagnostic);
            }
        }
        let expanded = expand(
            arguments,
            quote!(
                struct Feature<R> {
                    #[reducer(data)]
                    body: R,
                    #[reducer(data)]
                    metadata: usize,
                }
            ),
        )
        .expect("data markers do not request automatic reduction");
        let file = syn::parse2::<syn::File>(expanded).unwrap();
        let syn::Item::Struct(storage) = &file.items[0] else {
            panic!("expansion must retain the supplied storage");
        };
        assert!(storage.fields.iter().all(|field| field.attrs.is_empty()));
        assert_eq!(storage.fields.len(), 2);
    }

    #[test]
    fn expansion_rejects_invalid_helpers_after_valid_helpers() {
        for (child_signature, case_signature, diagnostic) in [
            (
                quote!(fn bind_optional(child: Child)),
                quote!(fn bind_alert(parent: Parent)),
                Some("binding needs 2 typed parameters and no return annotation"),
            ),
            (
                quote!(fn bind_optional(parent: Parent, child: Child)),
                quote!(fn bind_alert()),
                Some("binding needs 1 typed parameters and no return annotation"),
            ),
            (
                quote!(fn bind_optional(child: Child)),
                quote!(fn bind_alert()),
                Some("binding needs 2 typed parameters and no return annotation"),
            ),
            (
                quote!(fn bind_optional(parent: Parent, child: Child)),
                quote!(fn bind_alert(parent: Parent)),
                None,
            ),
        ] {
            let result = expand(
                quote!(
                    state(supplied(State)), action(supplied(Action)), body(supplied(body)),
                    child(required, reducer = Child, kind = required,
                        state = state_path, action = action_path,
                        bind(fn bind_required(child: Child))),
                    child(optional, reducer = Child, kind = optional,
                        state = state_path, action = action_path, bind(#child_signature)),
                    case(alert, policy = ephemeral, action = supplied(AlertAction),
                        state_path = state_path, action_path = action_path,
                        identity = identity, predicate = predicate, bind(#case_signature))
                ),
                quote!(
                    struct Feature<R> {
                        body: R,
                    }
                ),
            );
            if let Some(diagnostic) = diagnostic {
                assert_eq!(
                    result.expect_err("no partial helper output").to_string(),
                    diagnostic
                );
            } else {
                let file = syn::parse2::<syn::File>(result.unwrap()).unwrap();
                let helpers: Vec<_> = file
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        syn::Item::Fn(helper) => Some(helper.sig.ident.to_string()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(helpers, ["bind_required", "bind_optional", "bind_alert"]);
            }
        }
    }
    #[test]
    fn selected_child_storage_rejects_malformed_cfg_attr_arguments() {
        let error = expand(
            quote!(
                state(supplied(State)), action(supplied(Action)), body(generated),
                child(child, reducer = Child, kind = required,
                    state = state_path, action = action_path,
                    bind(pub fn bind_child(child: Child)))
            ),
            quote!(
                struct Feature<C> {
                    #[cfg_attr(unix cfg(windows))]
                    #[reducer(child = child)]
                    child: C,
                }
            ),
        )
        .expect_err("selected child storage cannot discard malformed conditions");
        assert_eq!(error.to_string(), "expected `,`");
    }

    #[test]
    fn nested_cfg_attr_preserves_storage_and_gates_generated_items() {
        let condition: proc_macro2::TokenStream =
            "#[cfg_attr(unix, cfg(feature = \"selected\"), doc = \"conditional child\")]"
                .parse()
                .expect("parseable attribute tokens");
        let expanded = expand(
            quote!(
                state(supplied(State)), action(supplied(Action)), body(generated),
                #condition
                child(child, reducer = Child, kind = required,
                    state = state_path, action = action_path,
                    bind(pub fn bind_child(child: Child)))
            ),
            quote!(struct Feature<R> {
                #condition
                #[reducer(child = child)]
                child: R,
                #[reducer(data)]
                marker: ::core::marker::PhantomData<R>,
            }),
        )
        .expect("valid nested conditions expand")
        .to_string();
        assert!(
            expanded.contains(
                &quote!(
                    struct Feature<R> {
                        #[cfg_attr(unix, cfg(feature = "selected"), doc = "conditional child")]
                        child: R,
                        marker: ::core::marker::PhantomData<R>,
                    }
                )
                .to_string()
            )
        );
        assert!(
            expanded.contains(
                &quote!(
                    #[cfg(any(not(unix), feature = "selected"))]
                    pub fn bind_child(child: Child)
                )
                .to_string()
            )
        );
        assert!(
            expanded.contains(
                &quote!(
                    #[cfg(any(not(unix), feature = "selected"))]
                    macro_rules! __rca_reducer_stage_0
                )
                .to_string()
            )
        );
        assert!(
            expanded.contains(
                &quote!(
                    #[cfg(not(any(not(unix), feature = "selected")))]
                    macro_rules! __rca_reducer_stage_0
                )
                .to_string()
            )
        );
    }
}
