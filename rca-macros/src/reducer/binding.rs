//! Emits ordinary once-constructed composition helpers selected by the caller.
//!
//! Helpers move their named arguments into concrete composition adapters. They
//! return `impl Reducer`, without boxing or adding `Clone`, `Send` or `'static` bounds.
//! The caller controls construction. Generated reduction uses the stored adapter.
//! Conditional attributes remain symbolic until the consumer compiles the helper.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{FnArg, Ident, Pat, ReturnType, Safety, Type};

use super::{
    cfg,
    syntax::{Binding, Case, Child},
};
use crate::canonical_ident;

/// Emits a helper that constructs the selected child composition.
///
/// Required children consume one child argument. Other kinds consume a parent
/// followed by a child. The local type annotation checks the selected reducer
/// type before construction, including any valid coercion from the argument.
///
/// # Errors
///
/// Returns an error for an unknown kind, invalid signature or malformed condition.
pub(super) fn child(
    child: &Child,
    state: &Type,
    action: &Type,
    rca: &TokenStream,
) -> syn::Result<TokenStream> {
    /// Selects an adapter and the number of owned arguments it consumes.
    enum Kind {
        /// Projects one child into its parent's domains without a parent reducer.
        Required,
        /// Composes a parent with a child held in optional state.
        Optional,
        /// Composes a parent with a child held by one enum case.
        Case,
        /// Composes a parent with an independently identified collection of children.
        Keyed,
    }

    let kind = canonical_ident(&child.kind);
    let (count, kind) = match kind.as_str() {
        "required" => (1, Kind::Required),
        "optional" => (2, Kind::Optional),
        "case" => (2, Kind::Case),
        "keyed" => (2, Kind::Keyed),
        _ => {
            return Err(syn::Error::new_spanned(
                &child.kind,
                "expected required, optional, case or keyed child composition",
            ));
        }
    };
    let parameters = parameters(&child.binding, count)?;
    let child_value = parameters[count - 1];
    let reducer = &child.reducer;
    let state_path = &child.state;
    let action_path = &child.action;
    let call = match kind {
        Kind::Required => quote!(#rca::scope(#state_path, #action_path, #child_value)),
        Kind::Optional => {
            let parent = parameters[0];
            quote!(#rca::if_some(#parent, #state_path, #action_path, #child_value))
        }
        Kind::Case => {
            let parent = parameters[0];
            quote!(#rca::if_case(#parent, #state_path, #action_path, #child_value))
        }
        Kind::Keyed => {
            let parent = parameters[0];
            quote!(#rca::for_each(#parent, #state_path, #action_path, #child_value))
        }
    };
    let condition = cfg::condition(&child.attributes)?;
    Ok(emit(
        &child.binding,
        state,
        action,
        rca,
        condition.as_ref(),
        &quote! {
            let #child_value: #reducer = #child_value;
            #call
        },
    ))
}

/// Emits an ephemeral composition helper, or nothing for a case without one.
///
/// The parser groups all required inputs, so emission needs no partial-input checks.
///
/// # Errors
///
/// Returns an error for an invalid signature or malformed condition.
pub(super) fn case(
    case: &Case,
    state: &Type,
    action: &Type,
    rca: &TokenStream,
) -> syn::Result<TokenStream> {
    let Some(reduction) = &case.reduction else {
        // A declaration without a helper must still reject malformed conditions.
        cfg::condition(&case.attributes)?;
        return Ok(TokenStream::new());
    };
    let binding = &reduction.binding;
    let parameters = parameters(binding, 1)?;
    let parent = parameters[0];
    let state_path = &reduction.state_path;
    let action_path = &reduction.action_path;
    let identity = &reduction.identity;
    let predicate = &reduction.predicate;
    let condition = cfg::condition(&case.attributes)?;
    Ok(emit(
        binding,
        state,
        action,
        rca,
        condition.as_ref(),
        &quote! {
            #rca::ephemeral(#parent, #state_path, #action_path, #identity, #predicate)
        },
    ))
}

/// Checks the signature and borrows the names of its consumed arguments.
///
/// Argument types can themselves contain references. This check excludes
/// reference patterns, not borrowed types supported by the selected adapter.
fn parameters(binding: &Binding, count: usize) -> syn::Result<Vec<&Ident>> {
    let signature = &binding.signature;
    if signature.inputs.len() != count || !matches!(signature.output, ReturnType::Default) {
        return Err(syn::Error::new_spanned(
            signature,
            format!("binding needs {count} typed parameters and no return annotation"),
        ));
    }
    if signature.asyncness.is_some()
        || signature.constness.is_some()
        || !matches!(signature.safety, Safety::Default)
        || signature.abi.is_some()
        || signature.variadic.is_some()
    {
        return Err(syn::Error::new_spanned(
            signature,
            "binding requires an ordinary synchronous Rust function",
        ));
    }
    signature
        .inputs
        .iter()
        .map(|argument| {
            let FnArg::Typed(parameter) = argument else {
                return Err(syn::Error::new_spanned(
                    argument,
                    "binding cannot take self",
                ));
            };
            let Pat::Ident(binding) = parameter.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &parameter.pat,
                    "binding requires named parameters",
                ));
            };
            if binding.by_ref.is_some() || binding.subpat.is_some() {
                return Err(syn::Error::new_spanned(
                    binding,
                    "binding must consume its named parameters",
                ));
            }
            Ok(&binding.ident)
        })
        .collect()
}

/// Preserves the caller's visibility, generics and bounds around the generated body.
fn emit(
    binding: &Binding,
    state: &Type,
    action: &Type,
    rca: &TokenStream,
    condition: Option<&TokenStream>,
    body: &TokenStream,
) -> TokenStream {
    let attributes = cfg::attribute(condition);
    let visibility = &binding.visibility;
    let signature = &binding.signature;
    let name = &signature.ident;
    let generics = &signature.generics;
    let where_clause = &generics.where_clause;
    let parameters = &signature.inputs;
    quote! {
        #attributes
        #visibility fn #name #generics (#parameters)
            -> impl #rca::Reducer<State = #state, Action = #action>
            #where_clause
        { #body }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reducer::syntax::Arguments;
    use quote::ToTokens;

    #[test]
    fn child_bindings_reject_invalid_kinds_and_signatures() {
        for (kind, signature, diagnostic) in [
            (
                "unknown",
                "fn bind(child: Child)",
                "expected required, optional, case or keyed child composition",
            ),
            (
                "required",
                "fn bind()",
                "binding needs 1 typed parameters and no return annotation",
            ),
            (
                "optional",
                "fn bind(child: Child)",
                "binding needs 2 typed parameters and no return annotation",
            ),
            (
                "required",
                "fn bind(child: Child) -> ()",
                "binding needs 1 typed parameters and no return annotation",
            ),
            (
                "required",
                "async fn bind(child: Child)",
                "binding requires an ordinary synchronous Rust function",
            ),
            (
                "required",
                "const fn bind(child: Child)",
                "binding requires an ordinary synchronous Rust function",
            ),
            (
                "required",
                "unsafe fn bind(child: Child)",
                "binding requires an ordinary synchronous Rust function",
            ),
            (
                "required",
                "extern \"C\" fn bind(child: Child)",
                "binding requires an ordinary synchronous Rust function",
            ),
            (
                "required",
                "fn bind(child: Child, ...)",
                "binding requires an ordinary synchronous Rust function",
            ),
            ("required", "fn bind(self)", "binding cannot take self"),
            (
                "required",
                "fn bind(_: Child)",
                "binding requires named parameters",
            ),
            (
                "required",
                "fn bind(ref child: Child)",
                "binding must consume its named parameters",
            ),
            (
                "required",
                "fn bind(child @ _: Child)",
                "binding must consume its named parameters",
            ),
        ] {
            let selected: Child = syn::parse_str(&format!(
                "child, reducer = Child, kind = {kind}, state = state_path, action = action_path, bind({signature})"
            ))
            .expect("parseable child selection");
            let error = child(
                &selected,
                &syn::parse_quote!(State),
                &syn::parse_quote!(Action),
                &quote!(::rca),
            )
            .expect_err("invalid binding");
            assert_eq!(error.to_string(), diagnostic, "{kind}: {signature}");
        }
    }

    #[test]
    fn child_kinds_emit_the_selected_composition_with_owned_parameters() {
        for (kind, parameters, expected) in [
            (
                "required",
                "child: Child",
                "::rca::scope(state_path, action_path, child)",
            ),
            (
                "optional",
                "parent: Parent, child: Child",
                "::rca::if_some(parent, state_path, action_path, child)",
            ),
            (
                "case",
                "parent: Parent, child: Child",
                "::rca::if_case(parent, state_path, action_path, child)",
            ),
            (
                "keyed",
                "parent: Parent, child: Child",
                "::rca::for_each(parent, state_path, action_path, child)",
            ),
        ] {
            let selected: Child = syn::parse_str(&format!(
                "child, reducer = Child, kind = {kind}, state = state_path, action = action_path, bind(pub fn bind({parameters}))"
            ))
            .expect("parseable child selection");
            let expanded = child(
                &selected,
                &syn::parse_quote!(State),
                &syn::parse_quote!(Action),
                &quote!(::rca),
            )
            .expect("valid binding");
            let function: syn::ItemFn = syn::parse2(expanded).expect("generated helper");
            let Some(syn::Stmt::Expr(expression, None)) = function.block.stmts.last() else {
                panic!("helper must return its selected composition");
            };
            let expected: syn::Expr = syn::parse_str(expected).unwrap();
            assert_eq!(
                expression.to_token_stream().to_string(),
                expected.to_token_stream().to_string()
            );
            assert_eq!(
                function.sig.output.to_token_stream().to_string(),
                quote!(-> impl ::rca::Reducer<State = State, Action = Action>).to_string()
            );
        }
    }

    #[test]
    fn case_bindings_require_one_owned_parent() {
        let selected: Case = syn::parse_str(
            "alert, policy = ephemeral, action = supplied(AlertAction),
             state_path = state_path, action_path = action_path,
             identity = identity, predicate = predicate, bind(fn bind())",
        )
        .expect("parseable ephemeral selection");
        let error = case(
            &selected,
            &syn::parse_quote!(State),
            &syn::parse_quote!(Action),
            &quote!(::rca),
        )
        .expect_err("missing parent parameter");
        assert_eq!(
            error.to_string(),
            "binding needs 1 typed parameters and no return annotation"
        );
    }

    #[test]
    fn nonreducing_cases_emit_no_binding() {
        for policy in ["ignored", "data"] {
            let selected: Case =
                syn::parse_str(&format!("alert, policy = {policy}, action = empty")).unwrap();
            assert!(
                case(
                    &selected,
                    &syn::parse_quote!(State),
                    &syn::parse_quote!(Action),
                    &quote!(::renamed_rca),
                )
                .unwrap()
                .is_empty()
            );
        }
    }

    #[test]
    fn ephemeral_binding_retains_signature_and_uses_every_selected_input() {
        let arguments: Arguments = syn::parse_str(
            "state(empty), action(empty), body(generated),
             #[cfg(feature = \"alerts\")]
             case(alert, policy = ephemeral, action = supplied(AlertAction<'a>),
                 state_path = state_path, action_path = action_path,
                 identity = identity, predicate = predicate,
                 bind(pub(crate) fn bind_alert<'a, P>(r#type: P)
                     where P: Reducer<State = State<'a>, Action = Action<'a>>))",
        )
        .unwrap();
        let expanded = case(
            &arguments.cases[0],
            &syn::parse_quote!(State<'a>),
            &syn::parse_quote!(Action<'a>),
            &quote!(::renamed_rca),
        )
        .unwrap();
        let expected = quote! {
            #[cfg(feature = "alerts")]
            pub(crate) fn bind_alert<'a, P>(r#type: P)
                -> impl ::renamed_rca::Reducer<State = State<'a>, Action = Action<'a>>
                where P: Reducer<State = State<'a>, Action = Action<'a>>
            {
                ::renamed_rca::ephemeral(r#type, state_path, action_path, identity, predicate)
            }
        };
        let expanded: syn::ItemFn = syn::parse2(expanded).expect("ordinary generated function");
        let expected: syn::ItemFn = syn::parse2(expected).unwrap();
        assert_eq!(
            expanded.to_token_stream().to_string(),
            expected.to_token_stream().to_string(),
        );
    }

    #[test]
    fn child_and_case_bindings_propagate_invalid_conditions() {
        let arguments: Arguments = syn::parse_str(
            "state(empty), action(empty), body(generated),
             #[cfg]
             child(child, reducer = Child, kind = required, state = state_path,
                 action = action_path, bind(fn bind_child(child: Child))),
             #[cfg]
             case(alert, policy = ephemeral, action = supplied(AlertAction),
                 state_path = state_path, action_path = action_path,
                 identity = identity, predicate = predicate, bind(fn bind_alert(parent: Parent)))",
        )
        .expect("attributes parse before condition validation");
        let state = syn::parse_quote!(State);
        let action = syn::parse_quote!(Action);
        let rca = quote!(::rca);
        for result in [
            child(&arguments.children[0], &state, &action, &rca),
            case(&arguments.cases[0], &state, &action, &rca),
        ] {
            assert_eq!(
                result.expect_err("malformed condition").to_string(),
                "expected cfg(predicate)"
            );
        }
    }
}
