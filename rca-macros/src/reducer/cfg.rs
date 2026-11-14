//! Preserves structural cfg conditions without evaluating the caller's configuration.
//!
//! Multiple `cfg` attributes require every predicate to hold. A nested
//! `cfg_attr(P, cfg(Q))` requires `Q` only when `P` holds, so its condition is
//! `any(not(P), Q)`. Non-structural payloads impose no inclusion condition.
//! Rust checks the emitted predicates in the consumer's compilation context.

use proc_macro2::TokenStream;
use quote::{ToTokens, quote};
use syn::{
    Attribute, LitBool, Meta, Token,
    parse::{ParseStream, discouraged::Speculative},
    punctuated::Punctuated,
};

use crate::path_is_ident;

/// Combines direct and nested conditional cfgs into the same inclusion predicate.
///
/// Returns `None` when the attributes contain no structural `cfg`. Predicates
/// retain source spans and are not evaluated against the macro host's configuration.
///
/// # Errors
///
/// Returns an error if conditional metadata has an invalid outer shape.
/// Rust checks emitted predicate syntax after expansion.
pub(super) fn condition(attributes: &[Attribute]) -> syn::Result<Option<TokenStream>> {
    let mut conditions = Vec::new();
    for attribute in attributes {
        if let Some(condition) = meta_condition(&attribute.meta)? {
            conditions.push(condition);
        }
    }
    Ok(conjunction(conditions))
}

/// Emits only structural conditional attributes, not unrelated `cfg_attr` payloads.
///
/// An unrestricted condition emits no attribute.
pub(super) fn attribute(condition: Option<&TokenStream>) -> TokenStream {
    condition.map_or_else(TokenStream::new, |condition| quote!(#[cfg(#condition)]))
}

/// Extracts a direct predicate or the implication of nested conditional attributes.
fn meta_condition(meta: &Meta) -> syn::Result<Option<TokenStream>> {
    if path_is_ident(meta.path(), "cfg") {
        let Meta::List(list) = meta else {
            return Err(syn::Error::new_spanned(meta, "expected cfg(predicate)"));
        };
        return Ok(Some(list.tokens.clone()));
    }
    if !path_is_ident(meta.path(), "cfg_attr") {
        return Ok(None);
    }
    let Meta::List(list) = meta else {
        return Err(syn::Error::new_spanned(
            meta,
            "expected cfg_attr(predicate, attributes)",
        ));
    };
    let (predicate, arguments) = list.parse_args_with(|input: ParseStream<'_>| {
        if input.is_empty() {
            return Err(syn::Error::new_spanned(
                meta,
                "cfg_attr requires a predicate and attributes",
            ));
        }
        // A cfg predicate also permits boolean literals, which are not Meta paths.
        // Keep the parsed literal; a failed attempt leaves Meta's cursor unchanged.
        let fork = input.fork();
        let predicate = if let Ok(predicate) = fork.parse::<LitBool>() {
            input.advance_to(&fork);
            predicate.into_token_stream()
        } else {
            input.parse::<Meta>()?.into_token_stream()
        };
        input.parse::<Token![,]>()?;
        let arguments = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        Ok((predicate, arguments))
    })?;
    let mut consequences = Vec::new();
    for argument in arguments {
        if let Some(condition) = meta_condition(&argument)? {
            consequences.push(condition);
        }
    }
    Ok(conjunction(consequences).map(|consequence| quote!(any(not(#predicate), #consequence))))
}

/// Joins predicates without adding an empty or single-element `all` wrapper.
fn conjunction(conditions: Vec<TokenStream>) -> Option<TokenStream> {
    match conditions.len() {
        0 => None,
        1 => conditions.into_iter().next(),
        _ => Some(quote!(all(#(#conditions),*))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_cfg_attr_predicates_preserve_their_consequences() {
        for predicate in [true, false] {
            let attributes = vec![syn::parse_quote!(#[cfg_attr(#predicate, cfg(unix))])];
            assert_eq!(
                condition(&attributes).unwrap().unwrap().to_string(),
                quote!(any(not(#predicate), unix)).to_string(),
            );
        }
    }

    #[test]
    fn mixed_nested_payloads_require_every_structural_consequence() {
        let attributes = vec![syn::parse_quote!(#[cfg_attr(
            true,
            cfg(unix),
            doc = "not structural",
            cfg_attr(false, cfg(windows)),
        )])];
        assert_eq!(
            condition(&attributes).unwrap().unwrap().to_string(),
            quote!(any(not(true), all(unix, any(not(false), windows)))).to_string(),
        );
    }

    #[test]
    fn empty_cfg_attr_payloads_are_unrestricted_but_require_a_separator() {
        for predicate in [quote!(true), quote!(false), quote!(unix)] {
            let attributes = vec![syn::parse_quote!(#[cfg_attr(#predicate,)])];
            assert!(condition(&attributes).unwrap().is_none());

            let malformed = vec![syn::parse_quote!(#[cfg_attr(#predicate)])];
            assert_eq!(
                condition(&malformed).unwrap_err().to_string(),
                "expected `,`",
            );
        }
    }

    #[test]
    fn malformed_cfg_attr_predicate_retains_its_expression_error() {
        let attributes = vec![syn::parse_quote!(#[cfg_attr(feature =, cfg(unix))])];
        assert_eq!(
            condition(&attributes).unwrap_err().to_string(),
            "expected an expression",
        );
    }

    #[test]
    fn malformed_cfg_attr_payload_retains_its_identifier_error() {
        let attributes = vec![syn::parse_quote!(#[cfg_attr(unix, =)])];
        assert_eq!(
            condition(&attributes).unwrap_err().to_string(),
            "expected identifier",
        );
    }

    #[test]
    fn attribute_emission_retains_the_condition_and_omits_unrestricted_attributes() {
        assert!(attribute(None).is_empty());
        let predicate = quote!(any(not(false), unix));
        assert_eq!(
            attribute(Some(&predicate)).to_string(),
            quote!(#[cfg(any(not(false), unix))]).to_string(),
        );
    }

    #[test]
    fn direct_and_nested_conditions_preserve_their_logical_implication() {
        let attributes: Vec<Attribute> = vec![
            syn::parse_quote!(#[cfg(feature = "outer")]),
            syn::parse_quote!(#[cfg_attr(feature = "first", cfg_attr(feature = "second", cfg(unix)))]),
        ];
        assert_eq!(
            condition(&attributes).unwrap().unwrap().to_string(),
            quote!(all(
                feature = "outer",
                any(not(feature = "first"), any(not(feature = "second"), unix))
            ))
            .to_string(),
        );
    }

    #[test]
    fn non_structural_attributes_are_not_copied_to_generated_methods() {
        let attributes: Vec<Attribute> = vec![
            syn::parse_quote!(#[derive(Debug)]),
            syn::parse_quote!(#[cfg_attr(feature = "docs", doc = "description")]),
        ];
        assert!(condition(&attributes).unwrap().is_none());
    }
}
