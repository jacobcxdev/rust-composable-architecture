use proc_macro::TokenStream;

use quote::quote;
use syn::{DataEnum, Fields, Ident};

use crate::util;

pub fn derive_macro(identifier: Ident, data: DataEnum) -> TokenStream {
    // For enums: route only into the *active* variant's inner reducer (if any).
    let child_reducers = data
        .variants
        .iter()
        .filter(|variant| {
            variant.attrs.iter().all(|attr| {
                !attr.path().is_ident("reducer")
                    || attr
                        .parse_args::<Ident>()
                        .map(|arg| arg != "skip")
                        .unwrap_or(true)
            })
        })
        .map(|variant| {
            let name = &variant.ident;

            // Only single-field tuple variants can participate as child reducers:
            // `Enum::Variant(ChildState)` or `Enum::Variant(KeyedState<…>)`.
            let keyed_state_ty = match &variant.fields {
                Fields::Unnamed(fields) if fields.unnamed.len() == 1 => Some(&fields.unnamed[0].ty),
                _ => None,
            };

            if keyed_state_ty.is_some_and(util::is_keyed_state) {
                let into_state = quote! { state };
                let recurse = util::keyed_child_reduce(into_state);

                quote! {
                    #identifier::#name(state) => {
                        #recurse
                    }
                }
            } else {
                quote! {
                    #identifier::#name(state) => {
                    // Standard variant routing: if the parent action can convert into the
                    // variant's child action, run it and scope effects back to the parent action.
                        if let Ok(action) = action.clone().try_into() {
                            composable::Reducer::reduce(state, action, send.scope());
                        }
                    }
                }
            }
        });

    let expanded = quote! {
        #[automatically_derived]
        impl composable::Reducer for #identifier
            where <Self as RecursiveReducer>::Action: Clone
        {
            type Action = <Self as RecursiveReducer>::Action;
            type Output = Self;

            fn reduce(
                &mut self,
                action: Self::Action,
                send: impl composable::Effects<Self::Action>,
            ) {
                // Parent runs first (pre-order traversal).
                <Self as RecursiveReducer>::reduce(self, action.clone(), send.clone());

                #[allow(unreachable_patterns)]
                match self {
                    #( #child_reducers )*
                    _ => {}
                }
            }
        }
    };

    TokenStream::from(expanded)
}
