use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{GenericArgument, PathArguments, Type, TypeParamBound};

/// Heuristically detect “keyed child state”.
///
/// This is intentionally a *best-effort* match over common type shapes. We prefer users wrap
/// collections in `composable::KeyedState<…>` (stable, explicit), but also recognise direct
/// `HashMap`/`BTreeMap` usage and common wrappers like `Option<_>`/`Box<_>`.
///
/// This function does not attempt to fully resolve type aliases or paths—proc-macros operate on
/// syntax, not type information—so the match is conservative and syntactic.
pub fn is_keyed_state(ty: &Type) -> bool {
    match ty {
        Type::Path(path) => {
            let Some(seg) = path.path.segments.last() else {
                return false;
            };

            match seg.ident.to_string().as_str() {
                "KeyedState" | "HashMap" | "BTreeMap" => true,
                "Box" | "Option" => {
                    let PathArguments::AngleBracketed(args) = &seg.arguments else {
                        return false;
                    };

                    args.args
                        .iter()
                        .find_map(|arg| match arg {
                            GenericArgument::Type(ty) => Some(ty),
                            _ => None,
                        })
                        .is_some_and(is_keyed_state)
                }
                _ => false,
            }
        }
        Type::TraitObject(obj) => obj.bounds.iter().any(|bound| match bound {
            TypeParamBound::Trait(trait_bound) => trait_bound
                .path
                .segments
                .last()
                .is_some_and(|seg| seg.ident == "KeyedState"),
            _ => false,
        }),
        Type::Reference(reference) => is_keyed_state(&reference.elem),
        Type::Group(group) => is_keyed_state(&group.elem),
        Type::Paren(paren) => is_keyed_state(&paren.elem),
        _ => false,
    }
}

/// Generate the routing code for a keyed child collection.
///
/// Semantics:
/// - If the parent action can `TryInto<Keyed<K, ChildAction>>`, we attempt to look up the child state by key.
/// - If the key is present, we run the child reducer and scope its effects back into the parent action type
///   using `send.scope_keyed(key)`.
/// - If the key is absent, the action is ignored (no panic).
///
/// Note: this relies on the parent action having exactly one conversion route from `Keyed<K, ChildAction>`,
/// otherwise `From`/`TryInto` coherence will fail or become ambiguous.
pub fn keyed_child_reduce(into_state: TokenStream2) -> TokenStream2 {
    quote! {
        if let Ok(keyed) = action.clone().try_into() {
            let composable::Keyed { key, action: child_action } = keyed;
            if let Some(child_state) = #into_state.get_mut(&key) {
                composable::Reducer::reduce(
                    child_state,
                    child_action,
                    send.scope_keyed(key),
                );
            }
        }
    }
}
