//! Generates payload-independent case metadata and typed borrowed field views.
//!
//! The derive entry point receives rustc's enabled variants and fields. Labels
//! and view formatting expose case names, never payload values or their traits.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Type};

use crate::canonical_ident;
use crate::state_paths::{
    declaration_generics, fresh_binding, generic_namespace, projection_lifetime, source_generics,
    source_type, type_arguments,
};

/// Emits the tag, view and catalogue together so their case order stays aligned.
///
/// Projection only borrows fields. View formatting and tag traits therefore do
/// not add trait bounds to payloads, including recursive and borrowed types.
#[expect(
    clippy::too_many_lines,
    reason = "case tags, borrowed views and matches are generated together"
)]
pub(crate) fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            input,
            "CaseDescriptors can only be derived for an enum",
        ));
    };
    let root = &input.ident;
    let name = canonical_ident(root);
    // Rustc can invoke derives on malformed `$crate` item names. Validate one
    // suffixed name before reusing its identifier characters with other suffixes.
    let mut tag: syn::Ident = syn::parse_str(&format!("{name}Case")).map_err(|_parse_error| {
        syn::Error::new_spanned(root, "CaseDescriptors cannot derive valid case names")
    })?;
    tag.set_span(root.span());
    let view = syn::Ident::new(&format!("{name}CaseRef"), root.span());
    let namespace = syn::Ident::new(&format!("{name}Cases"), root.span());
    let visibility = &input.vis;
    let root_arguments = type_arguments(&input.generics);
    let root_type: Type = syn::parse_quote!(#root #root_arguments);
    let generics = source_generics(&input.generics, &root_type);
    let (impl_generics, _, where_clause) = generics.split_for_impl();
    let arguments = type_arguments(&generics);
    let lifetime = projection_lifetime(input);
    let borrowed = data
        .variants
        .iter()
        .any(|variant| !variant.fields.is_empty());
    let mut view_generics = generics.clone();
    if borrowed {
        view_generics.params.insert(0, syn::parse_quote!(#lifetime));
    }
    let (view_declaration, view_where) = declaration_generics(&view_generics, &root_type);
    let view_arguments = type_arguments(&view_generics);
    let (view_impl_generics, _, view_impl_where) = view_generics.split_for_impl();
    let metadata = generic_namespace(input, &namespace, &root_type);
    let formatter = fresh_binding(input, "__rca_formatter");
    let source = fresh_binding(input, "__rca_source");
    let callback = fresh_binding(input, "__rca_label");
    let label_lifetime = projection_lifetime(&quote!(#input #view_generics));
    let case_count = data.variants.len();
    let mut tags = Vec::with_capacity(case_count);
    let mut members = Vec::with_capacity(case_count);
    let mut catalogue = Vec::with_capacity(case_count);
    let mut cases = Vec::with_capacity(case_count);
    let mut projections = Vec::with_capacity(case_count);
    let mut labels = Vec::with_capacity(case_count);
    let mut debug = Vec::with_capacity(case_count);
    let mut shielded = vec![formatter.clone(), source.clone(), callback.clone()];
    for variant in &data.variants {
        let case = &variant.ident;
        let attributes: Vec<_> = variant
            .attrs
            .iter()
            .filter(|attribute| {
                attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr")
            })
            .collect();
        let label = canonical_ident(case);
        let debug_label = format!("{view}::{label}");
        let bindings: Vec<_> = variant
            .fields
            .iter()
            .enumerate()
            .map(|(index, _)| fresh_binding(input, &format!("__rca_field_{index}")))
            .collect();
        for binding in &bindings {
            if !shielded.contains(binding) {
                shielded.push(binding.clone());
            }
        }
        let types: Vec<_> = variant
            .fields
            .iter()
            .map(|field| source_type(&field.ty, &root_type))
            .collect();
        let (member, pattern, projected, debug_pattern) = match &variant.fields {
            Fields::Unit => (
                quote!(#case),
                quote!(#root::#case),
                quote!(#view::#case),
                quote!(Self::#case),
            ),
            Fields::Unnamed(_) => (
                quote!(#case(#(&#lifetime #types),*)),
                quote!(#root::#case(#(#bindings),*)),
                quote!(#view::#case(#(#bindings),*)),
                quote!(Self::#case(..)),
            ),
            Fields::Named(fields) => {
                let names: Vec<_> = fields
                    .named
                    .iter()
                    .map(|field| field.ident.as_ref().expect("named field"))
                    .collect();
                (
                    quote!(#case { #(#names: &#lifetime #types),* }),
                    quote!(#root::#case { #(#names: #bindings),* }),
                    quote!(#view::#case { #(#names: #bindings),* }),
                    quote!(Self::#case { .. }),
                )
            }
        };
        tags.push(quote!(#(#attributes)* #case));
        members.push(quote!(#(#attributes)* #member));
        catalogue.push(quote!(#(#attributes)* #tag::#case));
        cases.push(quote!(#(#attributes)* #pattern => #tag::#case));
        projections.push(quote!(#(#attributes)* #pattern => #projected));
        labels.push(quote!(#(#attributes)* Self::#case => ::core::option::Option::Some(#label)));
        debug.push(quote!(#(#attributes)* #debug_pattern => #formatter.write_str(#debug_label)));
    }
    let project_generics = if borrowed {
        quote!(<#lifetime>)
    } else {
        quote!()
    };
    let source_reference = if borrowed {
        quote!(&#lifetime #root_type)
    } else {
        quote!(&#root_type)
    };
    // References remain inhabited even when their enum has no cases.
    let match_source = if data.variants.is_empty() {
        quote!(*#source)
    } else {
        quote!(#source)
    };
    let match_debug = if data.variants.is_empty() {
        quote!(*self)
    } else {
        quote!(self)
    };
    Ok(quote! {
        /// Identifies a case independently of payloads and diagnostic labels.
        #[derive(
            ::core::fmt::Debug,
            ::core::clone::Clone,
            ::core::marker::Copy,
            ::core::cmp::PartialEq,
            ::core::cmp::Eq,
            ::core::hash::Hash
        )]
        #visibility enum #tag {
            #(#tags),*
        }
        const _: () = {
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{#(drop as #shielded),*};
            impl #tag {
                /// Returns the generated diagnostic spelling of this case.
                #visibility fn label(self) -> ::core::option::Option<&'static str> {
                    match self {
                        #(#labels),*
                    }
                }
                /// Returns a caller-supplied optional diagnostic label.
                #visibility fn label_with<#label_lifetime>(
                    self,
                    #callback: impl ::core::ops::FnOnce(Self) -> ::core::option::Option<&#label_lifetime str>
                ) -> ::core::option::Option<&#label_lifetime str> {
                    #callback(self)
                }
            }
        };
        /// Borrows original case fields without payload trait bounds.
        ///
        /// Debug prints only case metadata, never payload contents.
        #visibility enum #view #view_declaration #view_where {
            #(#members),*
        }
        const _: () = {
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{#(drop as #shielded),*};
            impl #view_impl_generics ::core::fmt::Debug for #view #view_arguments #view_impl_where {
                fn fmt(&self, #formatter: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    match #match_debug {
                        #(#debug),*
                    }
                }
            }
        };
        #metadata
        const _: () = {
            #[allow(unused_imports, reason = "aliases shield generated bindings from caller constants")]
            use ::core::mem::{#(drop as #shielded),*};
            impl #impl_generics #namespace #arguments #where_clause {
                /// Lists enabled cases in source declaration order.
                #visibility const ALL: &'static [#tag] = &[#(#catalogue),*];
                /// Identifies the actual case without comparing its payload.
                #visibility fn case(#source: &#root_type) -> #tag {
                    match #match_source {
                        #(#cases),*
                    }
                }
                /// Projects typed references to the original case fields.
                #visibility fn project #project_generics (
                    #source: #source_reference
                ) -> #view #view_arguments {
                    match #match_source {
                        #(#projections),*
                    }
                }
            }
        };
    })
}

#[cfg(test)]
mod expansion_tests {
    #[test]
    fn conditional_variants_gate_every_generated_case_surface() {
        use quote::{ToTokens, quote};
        use syn::visit_mut::VisitMut;

        #[derive(Default)]
        struct Surfaces {
            variants: Vec<String>,
            arms: Vec<String>,
            catalogue: Vec<String>,
        }

        impl VisitMut for Surfaces {
            fn visit_variant_mut(&mut self, i: &mut syn::Variant) {
                let attributes = &i.attrs;
                self.variants.push(quote!(#(#attributes)*).to_string());
            }

            fn visit_arm_mut(&mut self, i: &mut syn::Arm) {
                let attributes = &i.attrs;
                self.arms.push(quote!(#(#attributes)*).to_string());
            }

            fn visit_expr_array_mut(&mut self, i: &mut syn::ExprArray) {
                for element in &i.elems {
                    let syn::Expr::Path(element) = element else {
                        panic!("case catalogue must contain case tags");
                    };
                    assert_eq!(
                        element.path.to_token_stream().to_string(),
                        "EventCase :: Ready"
                    );
                    let attributes = &element.attrs;
                    self.catalogue.push(quote!(#(#attributes)*).to_string());
                }
            }
        }

        // Raw expansion input retains conditions; rustc may evaluate them before a derive.
        let input = syn::parse_quote! {
            enum Event {
                #[cfg(feature = "selected")]
                #[cfg_attr(feature = "guarded", cfg(feature = "payload"))]
                #[doc = "source variant only"]
                Ready { value: u8, revision: u16 },
            }
        };
        let mut output = syn::parse2::<syn::File>(super::expand(&input).unwrap()).unwrap();
        let mut surfaces = Surfaces::default();
        surfaces.visit_file_mut(&mut output);
        let expected = quote! {
            #[cfg(feature = "selected")]
            #[cfg_attr(feature = "guarded", cfg(feature = "payload"))]
        }
        .to_string();

        // Tags and borrowed views must have the same domain as the source enum.
        assert_eq!(surfaces.variants, vec![expected.clone(); 2]);
        // label, Debug, case selection and borrowed projection must omit disabled cases.
        assert_eq!(surfaces.arms, vec![expected.clone(); 4]);
        assert_eq!(surfaces.catalogue, [expected]);
    }

    #[test]
    fn non_enum_domains_reject_without_emitting_a_case_catalogue() {
        for source in [
            "struct State;",
            "struct State { value: u8 }",
            "union State { value: u8 }",
        ] {
            let input = syn::parse_str(source).expect("valid Rust domain");
            let error = super::expand(&input).expect_err("only enums have case descriptors");
            assert_eq!(
                error.to_string(),
                "CaseDescriptors can only be derived for an enum",
                "{source}",
            );
        }
    }

    #[test]
    fn generic_raw_cases_preserve_visibility_and_borrowed_shapes() {
        let input = syn::parse_quote! {
            pub(crate) enum Event<T> where T: 'static {
                Idle,
                r#type(T),
                Pair { first: T, second: usize },
            }
        };
        insta::assert_snapshot!(super::expand(&input).unwrap().to_string());
    }

    #[test]
    fn empty_cases_do_not_invent_payload_lifetimes() {
        let input = syn::parse_quote!(
            enum Empty {}
        );
        insta::assert_snapshot!(super::expand(&input).unwrap().to_string());
    }
}
