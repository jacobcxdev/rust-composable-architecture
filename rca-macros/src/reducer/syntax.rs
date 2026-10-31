//! Parses explicit reducer selections, leaving Rust types and construction to callers.
//!
//! Required options become complete values before expansion. Conditional attributes
//! stay symbolic; emitters validate their shape without evaluating host features.
//! Rust types, expressions and signatures retain their tokens and source spans.

use syn::parse::{Parse, ParseStream};
use syn::{Attribute, Expr, Ident, Item, Signature, Token, Type, Visibility, parenthesized};

use crate::{canonical_ident, path_is_ident};

/// Holds the required domains and body alongside ordered child and case declarations.
pub(super) struct Arguments {
    /// Selects the reducer's existing, generated or empty state.
    pub state: Domain,
    /// Selects the reducer's existing, generated or empty action.
    pub action: Domain,
    /// Selects persistent storage used for reduction.
    pub body: Body,
    /// Keeps child declarations in source order within their shared name namespace.
    pub children: Vec<Child>,
    /// Keeps case declarations in source order within their shared name namespace.
    pub cases: Vec<Case>,
}

/// Chooses caller-owned syntax instead of inferring a domain from reducer storage.
pub(super) enum Domain {
    /// Uses the exact caller type without generating a declaration.
    Supplied(Box<Type>),
    /// Expands a caller declaration and its selected paths.
    Generated(Box<Item>),
    /// Uses unit for state or an uninhabited action type.
    Empty,
}

/// Chooses one persistent reducer or explicit composition of marked storage fields.
pub(super) enum Body {
    /// Refers to a named storage field, not a constructor expression.
    Supplied(Ident),
    /// Reduces selected storage fields in declaration order.
    Generated,
}

/// Retains a helper declaration whose ordinary function shape is checked during emission.
pub(super) struct Binding {
    /// Preserves the caller's visibility on the generated free function.
    pub visibility: Visibility,
    /// Retains generics, parameters and bounds; no function body is accepted.
    pub signature: Signature,
}

/// Stores every input needed to construct a child composition helper.
pub(super) struct Child {
    /// Carries only `cfg` and `cfg_attr` attributes, without evaluating their predicates.
    pub attributes: Vec<Attribute>,
    /// Connects domain and storage markers to this declaration.
    pub name: Ident,
    /// Supplies the child's exact reducer type for generated associated-type paths.
    pub reducer: Type,
    /// Selects a composition kind, validated by the binding emitter.
    pub kind: Ident,
    /// Supplies the parent-to-child state path expression.
    pub state: Expr,
    /// Supplies the parent-to-child action path expression.
    pub action: Expr,
    /// Declares the once-constructed composition helper.
    pub binding: Binding,
}

/// Selects a caller-supplied or uninhabited case action payload.
pub(super) enum CaseAction {
    /// Uses an uninhabited payload, so the generated case cannot carry an action.
    Empty,
    /// Retains the exact payload type for policy-dependent action lowering.
    Supplied(Box<Type>),
}

/// Stores a case after validating its policy and complete reduction inputs.
pub(super) struct Case {
    /// Carries symbolic `cfg` and `cfg_attr` conditions for generated case items.
    pub attributes: Vec<Attribute>,
    /// Connects domain and storage markers to this declaration.
    pub name: Ident,
    /// Selects the action payload independently of reduction.
    pub action: CaseAction,
    /// Exists only for ephemeral policy; ignored and data cases have no reduction.
    pub reduction: Option<Ephemeral>,
}

/// Keeps the required ephemeral inputs together after the parser checks them.
pub(super) struct Ephemeral {
    /// Projects the current ephemeral state from its parent.
    pub state_path: Expr,
    /// Projects the ephemeral action from its parent.
    pub action_path: Expr,
    /// Supplies the state identity used to reject stale actions.
    pub identity: Expr,
    /// Supplies the predicate that decides whether the action dismisses state.
    pub predicate: Expr,
    /// Declares the helper that wraps the caller's parent reducer.
    pub binding: Binding,
}

/// Marks a domain member or persistent storage field for explicit lowering.
pub(super) enum Selection {
    /// Refers to a named child declaration.
    Child(Ident),
    /// Refers to a named case declaration.
    Case(Ident),
    /// Reduces this storage field directly with the parent's domains.
    Reduce,
    /// Keeps a storage field as caller data without automatic reduction.
    Data,
}

impl Parse for Arguments {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let mut state = None;
        let mut action = None;
        let mut body = None;
        let mut children: Vec<Child> = Vec::new();
        let mut cases: Vec<Case> = Vec::new();
        while !input.is_empty() {
            let attributes = input.call(Attribute::parse_outer)?;
            for attribute in &attributes {
                if !path_is_ident(attribute.path(), "cfg")
                    && !path_is_ident(attribute.path(), "cfg_attr")
                {
                    return Err(syn::Error::new_spanned(
                        attribute,
                        "selection only supports cfg attributes",
                    ));
                }
            }
            let option: Ident = input.parse()?;
            let content;
            parenthesized!(content in input);
            match canonical_ident(&option).as_str() {
                "state" | "action" | "body" if !attributes.is_empty() => {
                    return Err(syn::Error::new_spanned(
                        option,
                        "domain and body selections cannot be conditional",
                    ));
                }
                "state" => set_once(&mut state, content.parse()?, &option)?,
                "action" => set_once(&mut action, content.parse()?, &option)?,
                "body" => set_once(&mut body, content.parse()?, &option)?,
                "child" => {
                    let mut child: Child = content.parse()?;
                    child.attributes = attributes;
                    check_name(&children, &cases, &child.name)?;
                    children.push(child);
                }
                "case" => {
                    let mut case: Case = content.parse()?;
                    case.attributes = attributes;
                    check_name(&children, &cases, &case.name)?;
                    cases.push(case);
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        option,
                        "expected state, action, body, child or case selection",
                    ));
                }
            }
            if !content.is_empty() {
                return Err(content.error("unexpected tokens in reducer selection"));
            }
            comma_or_end(input)?;
        }
        let mut helper_names = std::collections::HashSet::new();
        for binding in children.iter().map(|child| &child.binding).chain(
            cases
                .iter()
                .filter_map(|case| case.reduction.as_ref().map(|reduction| &reduction.binding)),
        ) {
            if !helper_names.insert(canonical_ident(&binding.signature.ident)) {
                return Err(syn::Error::new_spanned(
                    &binding.signature.ident,
                    "duplicate binding helper name",
                ));
            }
        }
        Ok(Self {
            state: state.ok_or_else(|| input.error("missing explicit state selection"))?,
            action: action.ok_or_else(|| input.error("missing explicit action selection"))?,
            body: body.ok_or_else(|| input.error("missing explicit body selection"))?,
            children,
            cases,
        })
    }
}

/// Rejects a repeated child or case name at its later declaration.
///
/// Raw and ordinary identifiers share the same namespace.
fn check_name(children: &[Child], cases: &[Case], name: &Ident) -> syn::Result<()> {
    let selected = canonical_ident(name);
    if children
        .iter()
        .any(|child| canonical_ident(&child.name) == selected)
        || cases
            .iter()
            .any(|case| canonical_ident(&case.name) == selected)
    {
        return Err(syn::Error::new_spanned(
            name,
            "duplicate child or case selection",
        ));
    }
    Ok(())
}

impl Parse for Domain {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let selection: Ident = input.parse()?;
        let choice = canonical_ident(&selection);
        if choice == "empty" {
            return Ok(Self::Empty);
        }
        let content;
        parenthesized!(content in input);
        let domain = match choice.as_str() {
            "supplied" => Self::Supplied(content.parse()?),
            "generated" => Self::Generated(content.parse()?),
            _ => {
                return Err(syn::Error::new_spanned(
                    selection,
                    "expected supplied(type), generated(item) or empty",
                ));
            }
        };
        if !content.is_empty() {
            return Err(content.error("expected one domain type or declaration"));
        }
        Ok(domain)
    }
}

impl Parse for Body {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let selection: Ident = input.parse()?;
        match canonical_ident(&selection).as_str() {
            "supplied" => {
                let content;
                parenthesized!(content in input);
                let field = content.parse()?;
                if !content.is_empty() {
                    return Err(content.error("supplied body requires one persistent field"));
                }
                Ok(Self::Supplied(field))
            }
            "generated" => Ok(Self::Generated),
            _ => Err(syn::Error::new_spanned(
                selection,
                "expected supplied(field) or generated body",
            )),
        }
    }
}

impl Parse for Binding {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let content;
        parenthesized!(content in input);
        let binding = Self {
            visibility: content.parse()?,
            signature: content.parse()?,
        };
        if !content.is_empty() {
            return Err(content.error("binding requires only an explicit function signature"));
        }
        Ok(binding)
    }
}

impl Parse for Child {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name = input.parse()?;
        input.parse::<Token![,]>()?;
        let mut reducer = None;
        let mut kind = None;
        let mut state = None;
        let mut action = None;
        let mut binding = None;
        while !input.is_empty() {
            let option: Ident = input.parse()?;
            let choice = canonical_ident(&option);
            if choice == "bind" {
                set_once(&mut binding, input.parse()?, &option)?;
            } else {
                input.parse::<Token![=]>()?;
                match choice.as_str() {
                    "reducer" => set_once(&mut reducer, input.parse()?, &option)?,
                    "kind" => set_once(&mut kind, input.parse()?, &option)?,
                    "state" => set_once(&mut state, input.parse()?, &option)?,
                    "action" => set_once(&mut action, input.parse()?, &option)?,
                    _ => return Err(syn::Error::new_spanned(option, "unknown child selection")),
                }
            }
            comma_or_end(input)?;
        }
        Ok(Self {
            attributes: Vec::new(),
            name,
            reducer: reducer.ok_or_else(|| input.error("missing explicit child reducer type"))?,
            kind: kind.ok_or_else(|| input.error("missing explicit child composition kind"))?,
            state: state.ok_or_else(|| input.error("missing explicit child state path"))?,
            action: action.ok_or_else(|| input.error("missing explicit child action path"))?,
            binding: binding.ok_or_else(|| input.error("missing explicit child bind signature"))?,
        })
    }
}

impl Parse for CaseAction {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let choice: Ident = input.parse()?;
        match canonical_ident(&choice).as_str() {
            "empty" => Ok(Self::Empty),
            "supplied" => {
                let content;
                parenthesized!(content in input);
                let ty = content.parse()?;
                if !content.is_empty() {
                    return Err(content.error("expected one supplied action type"));
                }
                Ok(Self::Supplied(ty))
            }
            _ => Err(syn::Error::new_spanned(
                choice,
                "case action requires empty or supplied(type)",
            )),
        }
    }
}

impl Parse for Case {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name = input.parse()?;
        input.parse::<Token![,]>()?;
        let mut policy = None;
        let mut action = None;
        let mut state_path = None;
        let mut action_path = None;
        let mut identity = None;
        let mut predicate = None;
        let mut binding = None;
        while !input.is_empty() {
            let option: Ident = input.parse()?;
            let choice = canonical_ident(&option);
            if choice == "bind" {
                set_once(&mut binding, input.parse()?, &option)?;
            } else {
                input.parse::<Token![=]>()?;
                match choice.as_str() {
                    "policy" => set_once(&mut policy, input.parse()?, &option)?,
                    "action" => set_once(&mut action, input.parse()?, &option)?,
                    "state_path" => set_once(&mut state_path, input.parse()?, &option)?,
                    "action_path" => set_once(&mut action_path, input.parse()?, &option)?,
                    "identity" => set_once(&mut identity, input.parse()?, &option)?,
                    "predicate" => set_once(&mut predicate, input.parse()?, &option)?,
                    _ => return Err(syn::Error::new_spanned(option, "unknown case selection")),
                }
            }
            comma_or_end(input)?;
        }
        let policy: Ident = policy.ok_or_else(|| input.error("missing explicit case policy"))?;
        let action = action.ok_or_else(|| input.error("missing explicit case action selection"))?;
        let reduction = match canonical_ident(&policy).as_str() {
            "ignored" | "data" => {
                if state_path.is_some()
                    || action_path.is_some()
                    || identity.is_some()
                    || predicate.is_some()
                    || binding.is_some()
                {
                    return Err(syn::Error::new_spanned(
                        &policy,
                        "ignored/data policy cannot select a binding or reduction",
                    ));
                }
                None
            }
            "ephemeral" => {
                if matches!(action, CaseAction::Empty) {
                    return Err(syn::Error::new_spanned(
                        &policy,
                        "ephemeral policy requires supplied(action type)",
                    ));
                }
                let (
                    Some(state_path),
                    Some(action_path),
                    Some(identity),
                    Some(predicate),
                    Some(binding),
                ) = (state_path, action_path, identity, predicate, binding)
                else {
                    return Err(syn::Error::new_spanned(
                        &policy,
                        "ephemeral policy requires state_path, action_path, identity, predicate and bind",
                    ));
                };
                Some(Ephemeral {
                    state_path,
                    action_path,
                    identity,
                    predicate,
                    binding,
                })
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &policy,
                    "expected ignored, data or ephemeral policy",
                ));
            }
        };
        Ok(Self {
            attributes: Vec::new(),
            name,
            action,
            reduction,
        })
    }
}

/// Parses one optional reducer marker and removes only successfully consumed attributes.
///
/// Unrelated attributes retain their order. Errors leave the entire input unchanged.
///
/// # Errors
///
/// Returns an error for empty, malformed, unknown, repeated or conflicting markers.
pub(super) fn selection(attributes: &mut Vec<Attribute>) -> syn::Result<Option<Selection>> {
    let mut selected = None;
    for attribute in attributes
        .iter()
        .filter(|attribute| path_is_ident(attribute.path(), "reducer"))
    {
        if let syn::Meta::List(list) = &attribute.meta
            && list.tokens.is_empty()
        {
            return Err(syn::Error::new_spanned(
                attribute,
                "expected child, case, reduce or data selection",
            ));
        }
        attribute.parse_nested_meta(|meta| {
            if selected.is_some() {
                return Err(meta.error("conflicting or duplicate reducer selection"));
            }
            selected = Some(
                match meta.path.get_ident().map(canonical_ident).as_deref() {
                    Some("child") => Selection::Child(meta.value()?.parse()?),
                    Some("case") => Selection::Case(meta.value()?.parse()?),
                    Some("reduce") => Selection::Reduce,
                    Some("data") => Selection::Data,
                    _ => return Err(meta.error("expected child, case, reduce or data selection")),
                },
            );
            Ok(())
        })?;
    }
    attributes.retain(|attribute| !path_is_ident(attribute.path(), "reducer"));
    Ok(selected)
}

/// Records a parsed option, reporting duplicates at the repeated option's name.
fn set_once<T>(slot: &mut Option<T>, value: T, name: &Ident) -> syn::Result<()> {
    if slot.replace(value).is_some() {
        return Err(syn::Error::new_spanned(
            name,
            format!("duplicate `{name}` selection"),
        ));
    }
    Ok(())
}

/// Requires a separator when tokens remain, permitting one trailing comma.
fn comma_or_end(input: ParseStream<'_>) -> syn::Result<()> {
    if !input.is_empty() {
        input.parse::<Token![,]>()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Arguments, Binding, Body, Case, CaseAction, Child, Domain, Selection, selection};
    use syn::Attribute;
    use syn::parse::{Parse, Parser};

    fn rejects<T: Parse>(source: &str, diagnostic: &str) {
        let error = syn::parse_str::<T>(source)
            .err()
            .unwrap_or_else(|| panic!("accepted invalid selection: {source}"));
        assert!(
            error.to_string().contains(diagnostic),
            "{source}: expected {diagnostic:?}, got {error}"
        );
    }

    #[test]
    fn domain_empty_accepts_raw_keywords() {
        for source in ["empty", "r#empty"] {
            assert!(matches!(
                syn::parse_str::<Domain>(source).unwrap(),
                Domain::Empty
            ));
        }
    }

    #[test]
    fn raw_conditional_attribute_names_are_accepted() {
        for attribute in ["#[r#cfg(false)]", "#[r#cfg_attr(true, r#cfg(false))]"] {
            let source = format!(
                "state(empty), action(empty), body(generated),
                 {attribute} case(alert, policy = ignored, action = empty)"
            );
            syn::parse_str::<Arguments>(&source).unwrap();
        }
    }

    #[test]
    fn raw_conditional_attribute_names_preserve_their_conditions() {
        for (source, expected) in [
            ("#[r#cfg(false)]", quote::quote!(false)),
            (
                "#[r#cfg_attr(true, r#cfg(false))]",
                quote::quote!(any(not(true), false)),
            ),
        ] {
            let attributes = Attribute::parse_outer.parse_str(source).unwrap();
            let actual = super::super::cfg::condition(&attributes).unwrap().unwrap();
            assert_eq!(actual.to_string(), expected.to_string());
        }
    }

    #[test]
    fn raw_conditions_gate_every_aggregate_state_surface() {
        let input = syn::parse_quote! {
            enum State {
                #[r#cfg(false)]
                Missing(u8, u16),
            }
        };
        let expanded = crate::state_paths::expand(&input).unwrap();
        let output = syn::parse2::<syn::File>(expanded).unwrap();
        let [
            syn::Item::Struct(_),
            syn::Item::Struct(marker),
            syn::Item::Const(gate),
            syn::Item::Impl(paths),
        ] = output.items.as_slice()
        else {
            panic!("expected the aggregate namespace, marker, implementations and constructors");
        };
        let has_condition = |attributes: &[Attribute]| {
            attributes.iter().any(|attribute| {
                quote::quote!(#attribute).to_string() == quote::quote!(#[r#cfg(false)]).to_string()
            })
        };
        assert!(has_condition(&marker.attrs));
        assert!(has_condition(&gate.attrs));
        let [syn::ImplItem::Fn(constructor)] = paths.items.as_slice() else {
            panic!("expected the aggregate constructor");
        };
        assert!(has_condition(&constructor.attrs));
        let syn::Expr::Block(block) = gate.expr.as_ref() else {
            panic!("expected the gated implementation block");
        };
        let mutable = block
            .block
            .stmts
            .iter()
            .find_map(|statement| match statement {
                syn::Stmt::Item(syn::Item::Impl(item))
                    if item.trait_.as_ref().is_some_and(|(path, _)| {
                        path.segments.last().unwrap().ident == "MutableAggregateCase"
                    }) =>
                {
                    Some(item)
                }
                _ => None,
            })
            .expect("mutable aggregate implementation retains its condition");
        assert!(has_condition(&mutable.attrs));
    }

    #[test]
    fn binding_options_accept_raw_keywords() {
        for keyword in ["bind", "r#bind"] {
            let child = syn::parse_str::<Child>(&format!(
                "counter, reducer = Counter, kind = required, state = state, action = action,
                 {keyword}(pub fn bind_counter<'a, R>(child: &'a R) where R: 'a)"
            ))
            .unwrap();
            assert_eq!(child.binding.signature.ident, "bind_counter");
            assert_eq!(child.binding.signature.generics.params.len(), 2);
            assert!(child.binding.signature.generics.where_clause.is_some());
            let case = syn::parse_str::<Case>(&format!(
                "alert, policy = ephemeral, action = supplied(AlertAction),
                 state_path = state, action_path = action, identity = identity, predicate = predicate,
                 {keyword}(pub fn bind_alert(parent: Parent))"
            ))
            .unwrap();
            assert_eq!(
                case.reduction.unwrap().binding.signature.ident,
                "bind_alert"
            );
        }
    }

    #[test]
    fn supplied_body_resolves_raw_and_ordinary_field_names() {
        for reference in ["body", "r#body"] {
            for field in ["body", "r#body"] {
                let arguments = syn::parse_str::<Arguments>(&format!(
                    "state(empty), action(empty), body(supplied({reference}))"
                ))
                .unwrap();
                let mut storage =
                    syn::parse_str(&format!("struct Feature<R> {{ {field}: R }}")).unwrap();
                let (selected, errors) =
                    super::super::select_fields(&mut storage, &arguments).unwrap();
                assert_eq!(selected.len(), 1);
                assert!(errors.is_empty());
            }
        }
    }

    #[test]
    fn nonreducing_cases_reject_malformed_conditional_attributes() {
        for policy in ["ignored", "data"] {
            for (attribute, diagnostic) in [
                ("#[cfg]", "expected cfg(predicate)"),
                ("#[cfg_attr]", "expected cfg_attr(predicate, attributes)"),
                (
                    "#[cfg_attr()]",
                    "cfg_attr requires a predicate and attributes",
                ),
            ] {
                let arguments = format!(
                    "state(empty), action(empty), body(generated),
                     {attribute} case(alert, policy = {policy}, action = empty)"
                );
                let error = super::super::expand(
                    arguments.parse().unwrap(),
                    quote::quote!(
                        struct Feature {}
                    ),
                )
                .unwrap_err();
                assert!(
                    error.to_string().contains(diagnostic),
                    "{arguments}: {error}"
                );
            }
        }
    }

    #[test]
    fn nonreducing_case_conditions_remain_symbolic() {
        for policy in ["ignored", "data"] {
            for attribute in [
                "#[cfg(true)]",
                "#[cfg(false)]",
                "#[cfg_attr(true, cfg(false))]",
                "#[cfg_attr(false, cfg(true))]",
            ] {
                let arguments = format!(
                    "state(empty), action(empty), body(generated),
                     {attribute} case(alert, policy = {policy}, action = empty)"
                );
                let expanded = super::super::expand(
                    arguments.parse().unwrap(),
                    quote::quote!(
                        struct Feature {}
                    ),
                )
                .unwrap();
                syn::parse2::<syn::File>(expanded).unwrap();
            }
        }
    }

    #[test]
    fn arguments_require_each_explicit_selection() {
        for (source, diagnostic) in [
            ("", "missing explicit state selection"),
            ("state(empty)", "missing explicit action selection"),
            (
                "state(empty), action(empty)",
                "missing explicit body selection",
            ),
        ] {
            rejects::<Arguments>(source, diagnostic);
        }
        let arguments =
            syn::parse_str::<Arguments>("state(empty), action(empty), body(generated),")
                .expect("all explicit selections with a trailing comma are valid");
        assert!(matches!(arguments.state, Domain::Empty));
        assert!(matches!(arguments.action, Domain::Empty));
        assert!(matches!(arguments.body, Body::Generated));
    }

    #[test]
    fn arguments_reject_unknown_conditional_and_repeated_selections() {
        for (source, diagnostic) in [
            (
                "#[doc = \"state\"] state(empty)",
                "selection only supports cfg attributes",
            ),
            (
                "#[cfg(any())] state(empty)",
                "domain and body selections cannot be conditional",
            ),
            (
                "#[cfg_attr(unix, cfg(any()))] action(empty)",
                "domain and body selections cannot be conditional",
            ),
            (
                "#[cfg(unix)] body(generated)",
                "domain and body selections cannot be conditional",
            ),
            (
                "unknown(empty)",
                "expected state, action, body, child or case selection",
            ),
            (
                "state(empty extra)",
                "unexpected tokens in reducer selection",
            ),
            ("state(empty), state(empty)", "duplicate `state` selection"),
            (
                "action(empty), action(empty)",
                "duplicate `action` selection",
            ),
            (
                "body(generated), body(generated)",
                "duplicate `body` selection",
            ),
            ("state(empty) action(empty)", "expected `,`"),
        ] {
            rejects::<Arguments>(source, diagnostic);
        }
    }

    #[test]
    fn nested_selections_reject_unknown_choices_and_extra_tokens() {
        for (source, diagnostic) in [
            (
                "unknown(State)",
                "expected supplied(type), generated(item) or empty",
            ),
            (
                "supplied(State, Action)",
                "expected one domain type or declaration",
            ),
            (
                "generated(struct State; struct Other;)",
                "expected one domain type or declaration",
            ),
        ] {
            rejects::<Domain>(source, diagnostic);
        }
        rejects::<Body>("unknown", "expected supplied(field) or generated body");
        rejects::<Body>(
            "supplied(first, second)",
            "supplied body requires one persistent field",
        );
        rejects::<Binding>(
            "(fn bind() {})",
            "binding requires only an explicit function signature",
        );
        rejects::<CaseAction>("unknown", "case action requires empty or supplied(type)");
        rejects::<CaseAction>(
            "supplied(Action, Other)",
            "expected one supplied action type",
        );
    }

    #[test]
    fn malformed_nested_syntax_propagates_parse_errors() {
        for source in [
            "#[]", "1", "state()", "action()", "body()", "child()", "case()",
        ] {
            rejects::<Arguments>(source, "expected");
        }
        for source in ["", "supplied()", "generated(1)"] {
            rejects::<Domain>(source, "expected");
        }
        for source in ["", "supplied(1)"] {
            rejects::<Body>(source, "expected");
            rejects::<CaseAction>(source, "expected");
        }
        for source in ["(pub(in) fn bind())", "(struct Binding;)"] {
            rejects::<Binding>(source, "expected");
        }
        for source in [
            "1,",
            "child",
            "child, 1",
            "child, bind()",
            "child, reducer Counter",
            "child, reducer = 1",
            "child, kind = 1",
            "child, state = @",
            "child, action = @",
            "child, reducer = Counter kind = required",
        ] {
            rejects::<Child>(source, "expected");
        }
        for source in [
            "1,",
            "alert",
            "alert, 1",
            "alert, bind()",
            "alert, policy ignored",
            "alert, policy = 1",
            "alert, action = 1",
            "alert, state_path = @",
            "alert, action_path = @",
            "alert, identity = @",
            "alert, predicate = @",
            "alert, policy = ignored action = empty",
        ] {
            rejects::<Case>(source, "expected");
        }
    }

    #[test]
    fn children_require_every_composition_option() {
        for (source, diagnostic) in [
            ("counter,", "missing explicit child reducer type"),
            (
                "counter, reducer = Counter",
                "missing explicit child composition kind",
            ),
            (
                "counter, reducer = Counter, kind = required",
                "missing explicit child state path",
            ),
            (
                "counter, reducer = Counter, kind = required, state = state_path",
                "missing explicit child action path",
            ),
            (
                "counter, reducer = Counter, kind = required, state = state_path, action = action_path",
                "missing explicit child bind signature",
            ),
            ("counter, unknown = Counter", "unknown child selection"),
        ] {
            rejects::<Child>(source, diagnostic);
        }
    }

    #[test]
    fn children_reject_repeated_options() {
        let source = "counter, reducer = Counter, kind = required, state = state_path, action = action_path, bind(fn bind_counter())";
        for (duplicate, option) in [
            ("reducer = Other", "reducer"),
            ("kind = optional", "kind"),
            ("state = other_state", "state"),
            ("action = other_action", "action"),
            ("bind(fn other())", "bind"),
        ] {
            rejects::<Child>(
                &format!("{source}, {duplicate}"),
                &format!("duplicate `{option}` selection"),
            );
        }
    }

    #[test]
    fn cases_require_an_explicit_supported_policy_and_action() {
        for (source, diagnostic) in [
            ("alert,", "missing explicit case policy"),
            (
                "alert, policy = ignored",
                "missing explicit case action selection",
            ),
            (
                "alert, policy = unknown, action = empty",
                "expected ignored, data or ephemeral policy",
            ),
            ("alert, unknown = value", "unknown case selection"),
            (
                "alert, policy = ephemeral, action = empty",
                "ephemeral policy requires supplied(action type)",
            ),
        ] {
            rejects::<Case>(source, diagnostic);
        }
    }

    #[test]
    fn nonreducing_cases_reject_every_reduction_option() {
        for policy in ["ignored", "data"] {
            for option in [
                "state_path = state",
                "action_path = action",
                "identity = identity",
                "predicate = predicate",
                "bind(fn bind_alert())",
            ] {
                rejects::<Case>(
                    &format!("alert, policy = {policy}, action = empty, {option}"),
                    "ignored/data policy cannot select a binding or reduction",
                );
            }
            let case = syn::parse_str::<Case>(&format!("alert, policy = {policy}, action = empty"))
                .expect("nonreducing cases need no reduction options");
            assert!(matches!(case.action, CaseAction::Empty));
            assert!(case.reduction.is_none());
        }
    }

    #[test]
    fn ephemeral_cases_require_every_reduction_option() {
        let options = [
            "state_path = state",
            "action_path = action",
            "identity = identity",
            "predicate = predicate",
            "bind(fn bind_alert())",
        ];
        for omitted in 0..options.len() {
            let remaining = options
                .iter()
                .enumerate()
                .filter_map(|(index, option)| (index != omitted).then_some(*option))
                .collect::<Vec<_>>()
                .join(", ");
            rejects::<Case>(
                &format!("alert, policy = ephemeral, action = supplied(AlertAction), {remaining}"),
                "ephemeral policy requires state_path, action_path, identity, predicate and bind",
            );
        }
        let case = syn::parse_str::<Case>(&format!(
            "alert, policy = ephemeral, action = supplied(AlertAction), {}",
            options.join(", ")
        ))
        .expect("complete ephemeral selections are valid");
        assert!(matches!(case.action, CaseAction::Supplied(_)));
        assert_eq!(
            case.reduction
                .expect("complete ephemeral inputs")
                .binding
                .signature
                .ident,
            "bind_alert"
        );
    }

    #[test]
    fn cases_reject_repeated_options() {
        for (option, name) in [
            ("policy = ignored", "policy"),
            ("action = empty", "action"),
            ("state_path = state", "state_path"),
            ("action_path = action", "action_path"),
            ("identity = identity", "identity"),
            ("predicate = predicate", "predicate"),
            ("bind(fn bind_alert())", "bind"),
        ] {
            rejects::<Case>(
                &format!("alert, {option}, {option}"),
                &format!("duplicate `{name}` selection"),
            );
        }
    }

    #[test]
    fn arguments_share_names_between_children_and_cases() {
        let child = "child(alert, reducer = Alert, kind = required, state = state, action = action, bind(fn bind_alert()))";
        let case = "case(r#alert, policy = ignored, action = empty)";
        for (first, second) in [(child, child), (child, case), (case, child), (case, case)] {
            rejects::<Arguments>(
                &format!("state(empty), action(empty), body(generated), {first}, {second}"),
                "duplicate child or case selection",
            );
        }
    }

    #[test]
    fn arguments_reject_shared_binding_names_across_children_and_cases() {
        rejects::<Arguments>(
            "state(empty), action(empty), body(generated),
             child(child, reducer = Child, kind = required, state = state, action = action,
                 bind(fn bind_shared())),
             case(alert, policy = ephemeral, action = supplied(AlertAction),
                 state_path = state, action_path = action, identity = identity, predicate = predicate,
                 bind(fn r#bind_shared()))",
            "duplicate binding helper name",
        );
    }

    #[test]
    fn selection_removes_only_reducer_attributes() {
        for (source, expected) in [
            ("#[reducer(child = r#type)]", ("child", "r#type")),
            ("#[reducer(r#child = r#type)]", ("child", "r#type")),
            ("#[reducer(case = alert)]", ("case", "alert")),
            ("#[reducer(r#case = alert)]", ("case", "alert")),
            ("#[reducer(reduce)]", ("reduce", "")),
            ("#[reducer(r#reduce)]", ("reduce", "")),
            ("#[reducer(data)]", ("data", "")),
            ("#[reducer(r#data)]", ("data", "")),
            ("#[r#reducer(data)]", ("data", "")),
            ("#[r#reducer(r#data)]", ("data", "")),
            ("", ("none", "")),
        ] {
            let mut attributes = Attribute::parse_outer
                .parse_str(&format!("#[doc = \"preserved\"] {source} #[cfg(unix)]"))
                .expect("valid attributes");
            let actual = match selection(&mut attributes).expect("one valid selection") {
                Some(Selection::Child(name)) => ("child", name.to_string()),
                Some(Selection::Case(name)) => ("case", name.to_string()),
                Some(Selection::Reduce) => ("reduce", String::new()),
                Some(Selection::Data) => ("data", String::new()),
                None => ("none", String::new()),
            };
            assert_eq!((actual.0, actual.1.as_str()), expected);
            assert_eq!(attributes.len(), 2);
            assert!(attributes[0].path().is_ident("doc"));
            assert!(attributes[1].path().is_ident("cfg"));
        }
    }

    #[test]
    fn qualified_helper_attributes_remain_unrelated() {
        for source in ["#[other::reducer(data)]", "#[::reducer(data)]"] {
            let mut attributes = Attribute::parse_outer.parse_str(source).unwrap();
            let original = quote::quote!(#(#attributes)*).to_string();
            assert!(selection(&mut attributes).unwrap().is_none());
            assert_eq!(quote::quote!(#(#attributes)*).to_string(), original);
        }
    }

    #[test]
    fn selection_rejects_conflicting_unknown_and_malformed_markers() {
        for (source, diagnostic) in [
            ("#[reducer]", "expected attribute arguments in parentheses"),
            ("#[reducer = data]", "expected parentheses"),
            (
                "#[reducer()]",
                "expected child, case, reduce or data selection",
            ),
            (
                "#[r#reducer()]",
                "expected child, case, reduce or data selection",
            ),
            (
                "#[reducer()] #[reducer(data)]",
                "expected child, case, reduce or data selection",
            ),
            (
                "#[reducer(data)] #[reducer()]",
                "expected child, case, reduce or data selection",
            ),
            (
                "#[reducer(reduce, data)]",
                "conflicting or duplicate reducer selection",
            ),
            (
                "#[reducer(data)] #[reducer(data)]",
                "conflicting or duplicate reducer selection",
            ),
            (
                "#[reducer(data)] #[r#reducer(r#data)]",
                "conflicting or duplicate reducer selection",
            ),
            (
                "#[reducer(unknown)]",
                "expected child, case, reduce or data selection",
            ),
            (
                "#[reducer(other::data)]",
                "expected child, case, reduce or data selection",
            ),
            ("#[reducer(child)]", "expected `=`"),
            ("#[reducer(case)]", "expected `=`"),
            ("#[reducer(child = 1)]", "expected identifier"),
            ("#[reducer(case = 1)]", "expected identifier"),
        ] {
            let mut attributes = Attribute::parse_outer
                .parse_str(source)
                .expect("valid attribute syntax");
            let original = quote::quote!(#(#attributes)*).to_string();
            let error = selection(&mut attributes)
                .err()
                .expect("invalid reducer marker");
            assert!(error.to_string().contains(diagnostic), "{source}: {error}");
            assert_eq!(
                quote::quote!(#(#attributes)*).to_string(),
                original,
                "failed parsing must not consume attributes"
            );
        }
    }
}
