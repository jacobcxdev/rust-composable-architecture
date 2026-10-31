//! Parses reducer expressions and delegates composition to the ordinary tuple API.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Expr, Token, parse::Parse, punctuated::Punctuated};

use crate::rca_path;

/// Emits one typed `combine` call, preserving expression order and evaluation count.
///
/// Rejects an empty list at its input span and propagates crate-resolution errors.
pub(crate) fn expand(input: &ReducersInput) -> syn::Result<TokenStream> {
    if input.expressions.is_empty() {
        return Err(syn::Error::new(
            input.span,
            "reducers! requires at least one reducer",
        ));
    }
    let rca = rca_path()?;
    let expressions = input.expressions.iter();
    Ok(quote!(#rca::combine((#(#expressions,)*))))
}

/// A fully parsed comma-separated list; malformed trailing tokens remain errors.
pub(crate) struct ReducersInput {
    /// Reducer expressions in declaration order, with an optional trailing comma.
    expressions: Punctuated<Expr, Token![,]>,
    /// Start of the original input, retained to locate an empty-list diagnostic.
    span: proc_macro2::Span,
}

impl Parse for ReducersInput {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let span = input.span();
        Ok(Self {
            expressions: input.parse_terminated(Expr::parse, Token![,])?,
            span,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ReducersInput, expand};

    #[test]
    fn empty_and_malformed_lists_do_not_silently_create_or_truncate_composition() {
        let empty: ReducersInput = syn::parse_str("").expect("empty list parses");
        assert_eq!(
            expand(&empty).unwrap_err().to_string(),
            "reducers! requires at least one reducer",
        );
        for source in [",", "First,,Second", "First;Second", "First, let"] {
            assert!(
                syn::parse_str::<ReducersInput>(source).is_err(),
                "invalid composition must not omit an expression: {source}",
            );
        }
    }

    #[test]
    fn public_reducer_expansion_matches_snapshot() {
        let input: ReducersInput = syn::parse_quote!(First, second(),);
        let actual = expand(&input).expect("expansion succeeds");
        insta::assert_snapshot!(actual.to_string());
    }

    #[test]
    fn single_reducer_expansion_keeps_the_tuple_comma() {
        let input: ReducersInput = syn::parse_quote!(Only);
        let actual = expand(&input).expect("expansion succeeds");
        insta::assert_snapshot!(actual.to_string());
    }
}
