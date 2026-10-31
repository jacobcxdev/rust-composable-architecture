#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

//! Provides optional derive syntax for Rust Composable Architecture.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, Type, parse_macro_input};
use toml_edit::{DocumentMut, Item, TableLike};

mod action_paths;
mod case_descriptors;
mod dependency_client;
mod reducer;
mod reducers;
/// Generates independent snapshot values from source fields and explicit capture hooks.
mod snapshot;
mod state_paths;

/// Captures independent owned field values without retaining source authority.
///
/// Generates a value-shaped companion with conditional Clone, equality and Debug.
/// Custom capture uses `#[snapshot(with = function, value = OwnedType)]`; omission
/// requires `#[snapshot(omit = "reason")]`. Redaction affects Debug, never equality.
/// `#[snapshot(name = History)]` renames the companion; `into = ExistingHistory`
/// uses an existing named-field struct whose traits remain user-owned.
///
/// Omitted tuple fields compact output indices. Source layout, discriminants and
/// unrelated helper attributes are not copied. Capture reads fields in declaration
/// order, not atomically. Custom captures must independently preserve both source
/// and expected-history independence; aliases cannot be checked by this derive.
/// Recursive fields require finite custom capture or a manual implementation.
/// A custom function pointer owns no captured environment. Its behavioural stability
/// and equality policy remain the caller's responsibility, as with any custom value.
///
/// # Examples
///
/// ```
/// use rca::Snapshot;
/// use rca_macros::Snapshot;
/// #[derive(Snapshot)]
/// struct Progress { completed: usize, history: Vec<String> }
/// let state = Progress { completed: 1, history: vec!["done".into()] };
/// let captured = state.snapshot();
/// assert_eq!(captured.completed, 1);
/// ```
#[proc_macro_derive(Snapshot, attributes(snapshot))]
pub fn derive_snapshot(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    snapshot::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Selects reducer domains and persistent body composition explicitly.
///
/// Supplied domains remain ordinary Rust types. Generated domains use ordinary
/// declarations and typed child paths. A supplied body delegates to its stored
/// field; generated bodies reduce selected persistent fields in declaration order.
/// Invalid or conflicting selections produce compile-time diagnostics.
///
/// # Examples
///
/// ```
/// use rca::{Effect, ReduceContext, Reducer};
/// use rca_macros::reducer;
/// struct Body;
/// impl Reducer for Body {
///     type State = ();
///     type Action = ();
///     fn reduce(&mut self, _: &mut (), _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
///         Effect::none()
///     }
/// }
/// #[reducer(state(supplied(())), action(supplied(())), body(supplied(body)))]
/// struct Feature { body: Body }
/// let feature = Feature { body: Body };
/// let _: &dyn Reducer<State = (), Action = ()> = &feature;
/// ```
#[proc_macro_attribute]
pub fn reducer(attribute: TokenStream, input: TokenStream) -> TokenStream {
    reducer::expand(attribute.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Generates an ordered case catalogue and typed borrowed case views.
///
/// For `Event`, this generates `EventCase`, `EventCaseRef` and `EventCases`.
/// `EventCases::ALL` retains cfg-enabled declaration order, including cases
/// skipped by path derives. Case labels are optional diagnostics, not identity.
/// `EventCases` is an uninhabited type-level namespace, not a diagnostic value.
/// Use its associated methods; case tags and borrowed views provide `Debug`.
/// Borrowed-view `Debug` prints only case metadata and requires no payload traits.
/// Existing discriminants, generics and visibility remain governed by Rust.
///
/// # Examples
/// ```
/// use rca_macros::CaseDescriptors;
/// #[derive(CaseDescriptors)]
/// enum Event { Idle, Value(String) }
/// let event = Event::Value(String::from("local"));
/// assert_eq!(EventCases::case(&event), EventCase::Value);
/// let EventCaseRef::Value(value) = EventCases::project(&event) else { unreachable!() };
/// assert_eq!(value, "local");
/// assert_eq!(EventCase::Idle.label_with(|_| None), None);
/// ```
#[proc_macro_derive(CaseDescriptors)]
pub fn derive_case_descriptors(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    case_descriptors::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

#[cfg(test)]
mod dependency_key_expansion_tests {
    #[test]
    fn key_storage_shape_is_rejected_before_default_options() {
        for declaration in [
            "struct Key(u8);",
            "struct Key { value: u8 }",
            "enum Key { Value }",
            "union Key { value: u8 }",
        ] {
            let input = syn::parse_str(&format!("#[dependency(unknown)] {declaration}"))
                .expect("the declaration and attribute tokens are valid Rust syntax");
            assert_eq!(
                super::expand_dependency_key(&input)
                    .expect_err("a dependency key must be a unit struct")
                    .to_string(),
                "DependencyKey can only be derived for a unit struct",
                "{declaration}"
            );
        }
    }

    #[test]
    fn default_sources_are_complete_and_unambiguous_before_expansion() {
        let cases = [
            (
                "#[doc = \"a dependency key\"]",
                "missing `#[dependency(value = Type, live = expression)]` attribute",
            ),
            (
                "#[dependency(value = u8, live = 1)] #[dependency(test = 2)]",
                "duplicate `dependency` attribute",
            ),
            (
                "#[dependency(live = 1, test = 2)]",
                "missing `value` dependency option",
            ),
            (
                "#[dependency(value = u8, test = 2)]",
                "missing `live` dependency option",
            ),
            (
                "#[dependency(value = u8, live = 1, test = 2, value)]",
                "duplicate `value` option",
            ),
            (
                "#[dependency(value = u8, live = 1, test = 2, live)]",
                "duplicate `live` option",
            ),
            (
                "#[dependency(value = u8, live = 1, test = 2, test)]",
                "duplicate `test` option",
            ),
            (
                "#[dependency(value = u8, live = 1, test = 2, preview = 3)]",
                "unknown dependency option; expected `value`, `live` or `test`",
            ),
        ];
        for (attributes, expected) in cases {
            let input = syn::parse_str(&format!("{attributes} struct Key;"))
                .expect("attributes retain their unparsed option tokens");
            assert_eq!(
                super::expand_dependency_key(&input)
                    .expect_err("invalid defaults must not generate an implementation")
                    .to_string(),
                expected,
                "{attributes}"
            );
        }
    }

    #[test]
    fn each_default_option_requires_assignment_and_valid_value_syntax() {
        let cases = [
            ("value, live = 1", "expected `=`"),
            ("value = u8, live, test = 2", "expected `=`"),
            ("value = u8, live = 1, test", "expected `=`"),
            ("value = *u8, live = 1", "expected `const` or `mut`"),
            ("value = u8, live = @, test = 2", "expected an expression"),
            ("value = u8, live = 1, test = @", "expected an expression"),
        ];
        for (options, expected) in cases {
            let input = syn::parse_str(&format!("#[dependency({options})] struct Key;"))
                .expect("malformed option values remain valid attribute token trees");
            assert_eq!(
                super::expand_dependency_key(&input)
                    .expect_err("each supplied option must parse before expansion")
                    .to_string(),
                expected,
                "{options}"
            );
        }
    }

    #[test]
    fn generic_raw_key_retains_factories_and_bounds() {
        let input = syn::parse_quote! {
            #[dependency(value = [u8; N], live = [7; N], test = [3; N])]
            pub(crate) struct r#type<const N: usize> where [u8; N]: Sized;
        };
        insta::assert_snapshot!(super::expand_dependency_key(&input).unwrap().to_string());
    }

    #[test]
    fn unit_key_omits_unrequested_test_factory() {
        let input = syn::parse_quote! {
            #[dependency(value = String, live = String::from("live"))]
            struct Key;
        };
        insta::assert_snapshot!(super::expand_dependency_key(&input).unwrap().to_string());
    }
}

#[cfg(test)]
mod identifier_tests {
    use quote::ToTokens;

    use super::{
        CrateOrigin, IdentifierClass, classify_identifier, dependency_origin,
        public_library_target, rust_ident,
    };
    use toml_edit::DocumentMut;

    #[test]
    fn type_references_alias_only_shadowing_type_parameters() {
        for declaration in [
            "struct Source;",
            "struct Source<Other>(Other);",
            "struct Source<const Source: usize>;",
        ] {
            let input: syn::DeriveInput = syn::parse_str(declaration).unwrap();
            let (reference, alias) = super::type_reference(
                &input,
                &input.generics,
                &input.generics,
                &input.ident,
                "Test",
            );
            assert_eq!(reference, input.ident);
            assert!(alias.is_empty());
        }
        for declaration in [
            "struct Source<Source>(Source);",
            "struct r#Source<Source>(Source);",
            "struct Source<r#Source>(Source);",
        ] {
            let input: syn::DeriveInput = syn::parse_str(declaration).unwrap();
            let (reference, alias) = super::type_reference(
                &input,
                &input.generics,
                &input.generics,
                &input.ident,
                "Test",
            );
            assert_eq!(reference, "__RcaTestSource");
            let alias: syn::ItemType = syn::parse2(alias).unwrap();
            assert_eq!(alias.ident, reference);
            assert_eq!(
                alias.generics.type_params().next().unwrap().ident,
                "__RcaTestSourceType0"
            );
            let name = &input.ident;
            let expected: syn::Type = syn::parse_quote!(#name<__RcaTestSourceType0>);
            assert_eq!(
                alias.ty.to_token_stream().to_string(),
                expected.to_token_stream().to_string()
            );
        }
        let input: syn::DeriveInput = syn::parse_quote! {
            struct Source<Source, __RcaTestSource>(Source, __RcaTestSource);
        };
        let (reference, _) = super::type_reference(
            &input,
            &input.generics,
            &input.generics,
            &input.ident,
            "Test",
        );
        assert_eq!(reference, "__RcaTestSource_");
        let (other, _) = super::type_reference(
            &input,
            &input.generics,
            &input.generics,
            &input.ident,
            "Other",
        );
        assert_eq!(other, "__RcaOtherSource");
    }

    #[test]
    fn type_aliases_preserve_arguments_without_copying_bounds_or_defaults() {
        let input: syn::DeriveInput = syn::parse_quote! {
            struct Source<'a: 'b, 'b, Source: Trait = DefaultValue, const N: usize = WIDTH>
            where Source: OtherTrait {
                value: &'a Source,
                bytes: &'b [u8; N],
                collision: __RcaTestSourceType2,
            }
        };
        let (_, alias) = super::type_reference(
            &input,
            &input.generics,
            &input.generics,
            &input.ident,
            "Test",
        );
        let alias: syn::ItemType = syn::parse2(alias).unwrap();
        let expected: syn::ItemType = syn::parse_quote! {
            #[doc(hidden)]
            type __RcaTestSource<'a, 'b, __RcaTestSourceType2_, const N: usize> =
                Source<'a, 'b, __RcaTestSourceType2_, { N }>;
        };
        assert_eq!(
            alias.to_token_stream().to_string(),
            expected.to_token_stream().to_string()
        );
        let (_, alias) = super::type_reference(
            &input,
            &input.generics,
            &syn::Generics::default(),
            &input.ident,
            "Test",
        );
        let alias: syn::ItemType = syn::parse2(alias).unwrap();
        assert!(alias.generics.params.is_empty());
        let expected: syn::Type = syn::parse_quote!(Source);
        assert_eq!(
            alias.ty.to_token_stream().to_string(),
            expected.to_token_stream().to_string()
        );
    }

    #[test]
    fn shadowed_types_keep_expansions_parseable_without_rewriting_input() {
        type Expander = fn(&syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream>;
        let cases: [(&str, Expander); 5] = [
            (
                "struct State<State, StatePaths> { value: identity!(State), other: StatePaths }",
                super::state_paths::expand,
            ),
            (
                "enum Action<Action, ActionPaths> { Value(Action), Pair(Action, ActionPaths) }",
                super::action_paths::expand,
            ),
            (
                "enum Event<Event, EventCase, EventCaseRef, EventCases> { Value(Event), Pair(EventCase, EventCaseRef, EventCases) }",
                super::case_descriptors::expand,
            ),
            (
                "struct Source<Source, SourceSnapshot> { value: identity!(Source), other: SourceSnapshot }",
                super::snapshot::expand,
            ),
            (
                "struct Client<Client> { #[dependency(member = ::core::marker::PhantomData)] marker: ::core::marker::PhantomData<Client> }",
                super::dependency_client::expand,
            ),
        ];
        for (source, expand) in cases {
            let input: syn::DeriveInput = syn::parse_str(source).unwrap();
            let original = input.to_token_stream().to_string();
            let _: syn::File = syn::parse2(expand(&input).unwrap()).unwrap();
            assert_eq!(input.to_token_stream().to_string(), original);
        }
    }

    #[test]
    fn classifies_generated_identifiers_without_overescaping_contextual_words() {
        let cases = [
            ("field", IdentifierClass::Plain, Some("field")),
            ("union", IdentifierClass::Plain, Some("union")),
            ("macro_rules", IdentifierClass::Plain, Some("macro_rules")),
            ("safe", IdentifierClass::Plain, Some("safe")),
            ("raw", IdentifierClass::Plain, Some("raw")),
            ("async", IdentifierClass::RawRequired, Some("r#async")),
            ("gen", IdentifierClass::RawRequired, Some("r#gen")),
            ("yield", IdentifierClass::RawRequired, Some("r#yield")),
            ("crate", IdentifierClass::Forbidden, None),
            ("self", IdentifierClass::Forbidden, None),
            ("Self", IdentifierClass::Forbidden, None),
            ("super", IdentifierClass::Forbidden, None),
        ];

        for (name, expected, generated) in cases {
            assert_eq!(classify_identifier(name), expected, "{name}");
            assert_eq!(
                rust_ident(name, proc_macro2::Span::call_site())
                    .ok()
                    .map(|ident| ident.to_string()),
                generated.map(str::to_owned),
                "{name}"
            );
            if expected == IdentifierClass::Forbidden {
                assert_eq!(
                    rust_ident(name, proc_macro2::Span::call_site())
                        .expect_err("forbidden names fail")
                        .to_string(),
                    format!(
                        "RCA derives cannot generate identifier `{name}` because it is not a legal raw Rust identifier"
                    )
                );
            }
        }
    }

    #[test]
    fn malformed_identifier_inputs_report_errors_without_panicking() {
        for name in ["", "_", "1", "$crate", "two words"] {
            assert_eq!(
                rust_ident(name, proc_macro2::Span::call_site())
                    .expect_err("invalid spelling is rejected")
                    .to_string(),
                format!(
                    "RCA derives cannot generate identifier `{name}` because it is not a legal Rust identifier"
                ),
            );
        }
    }

    #[test]
    fn snake_case_names_retain_identifiers_when_word_conversion_loses_them() {
        for (source, expected) in [
            ("SomeCase", "some_case"),
            ("r#type", "r#type"),
            ("__", "__"),
            ("__1", "__1"),
        ] {
            let source = syn::parse_str(source).expect("source is an identifier");
            assert_eq!(
                super::snake_case_ident(&source).unwrap().to_string(),
                expected
            );
        }
        let forbidden = syn::parse_quote!(Crate);
        super::snake_case_ident(&forbidden)
            .expect_err("the reserved lowercase crate name cannot become a method identifier");
    }

    #[test]
    fn manifest_reader_retains_package_and_library_metadata() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let manifest = super::read_manifest(&path).expect("the crate manifest is readable TOML");
        assert_eq!(manifest["package"]["name"].as_str(), Some("rca-macros"));
        assert_eq!(manifest["lib"]["proc-macro"].as_bool(), Some(true));
    }

    #[test]
    fn manifest_reader_distinguishes_file_access_from_toml_syntax_errors() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/manifest-reading");
        let missing = fixtures.join("missing.toml");
        assert_eq!(
            std::fs::metadata(&missing)
                .expect_err("the missing-file witness must remain absent")
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(
            fixtures.is_dir(),
            "the non-file witness must be a directory"
        );
        let malformed = fixtures.join("malformed.toml");
        let source = std::fs::read_to_string(&malformed)
            .expect("the malformed manifest must still be readable UTF-8");
        assert!(
            source.parse::<DocumentMut>().is_err(),
            "the malformed manifest must contain invalid TOML"
        );

        let read_error = "RCA macros could not read the active Cargo manifest";
        for (path, expected) in [
            (missing, read_error),
            (fixtures, read_error),
            (
                malformed,
                "RCA macros could not parse the active Cargo manifest",
            ),
        ] {
            assert_eq!(
                super::read_manifest(&path)
                    .expect_err("invalid manifest input must be diagnosed")
                    .to_string(),
                expected,
                "{}",
                path.display()
            );
        }
    }

    // Catches inventing a library target when Cargo context or dependency
    // provenance is absent. Child environments keep parallel tests isolated.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "isolated parent and child cases keep the manifest provenance matrix together"
    )]
    fn library_resolution_requires_context_only_for_ambiguous_canonical_names() {
        const CASE: &str = "RCA_MACROS_MANIFEST_CONTEXT_CASE";
        if let Ok(case) = std::env::var(CASE) {
            if case == "reserved-alias" {
                assert!(matches!(
                    proc_macro_crate::crate_name("rca-core")
                        .expect("the real manifest alias must resolve"),
                    proc_macro_crate::FoundCrate::Name(name) if name == "self",
                ));
                assert_eq!(
                    super::rca_path()
                        .expect_err("reserved aliases must be rejected without a panic")
                        .to_string(),
                    "RCA derives cannot generate identifier `self` because it is not a legal raw Rust identifier",
                );
                return;
            }
            let expected = match case.as_str() {
                "missing" => {
                    type Expander = fn(&syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream>;
                    let cases: [(&str, Expander); 3] = [
                        ("struct State<State>(State);", super::state_paths::expand),
                        (
                            "enum Action<Action> { Value(Action) }",
                            super::action_paths::expand,
                        ),
                        ("struct State<State>(State);", super::snapshot::expand),
                    ];
                    for (source, expand) in cases {
                        let input = syn::parse_str(source).unwrap();
                        assert_eq!(
                            expand(&input)
                                .expect_err("aliases cannot bypass dependency resolution")
                                .to_string(),
                            "RCA macros require `rust-composable-architecture` or `rca-core` in Cargo dependencies",
                        );
                    }
                    let error = super::reducer::expand(
                        quote::quote!(state(empty), action(empty), body(generated)),
                        quote::quote!(
                            struct Feature {}
                        ),
                    )
                    .expect_err("reducer attributes require an RCA dependency");
                    assert_eq!(
                        error.to_string(),
                        "RCA macros require `rust-composable-architecture` or `rca-core` in Cargo dependencies"
                    );
                    "RCA macros require Cargo manifest context"
                }
                "missing-manifest" => {
                    let manifest_dir = std::path::PathBuf::from(
                        std::env::var_os("CARGO_MANIFEST_DIR")
                            .expect("the missing-manifest case has Cargo context"),
                    );
                    assert!(
                        manifest_dir.is_dir(),
                        "the manifest fixture directory exists"
                    );
                    assert_eq!(
                        std::fs::metadata(manifest_dir.join("Cargo.toml"))
                            .expect_err("the fixture Cargo manifest must remain absent")
                            .kind(),
                        std::io::ErrorKind::NotFound
                    );
                    "RCA macros could not read the active Cargo manifest"
                }
                "unrelated" => {
                    "RCA macros could not determine the Cargo dependency origin for `rust-composable-architecture`"
                }
                "conflicting-origin" => {
                    "RCA macros found conflicting canonical and aliased Cargo dependency origins for `rust-composable-architecture`"
                }
                _ => panic!("unknown manifest context case"),
            };
            assert_eq!(
                super::public_library_target_for_manifest(
                    "rust-composable-architecture",
                    "rust_composable_architecture",
                )
                .expect_err("ambiguous names require Cargo provenance")
                .to_string(),
                expected
            );
            if case == "conflicting-origin" {
                // Exercise the real resolver boundary, not a compiled Cargo fixture.
                assert_eq!(
                    super::rca_path()
                        .expect_err("the resolver must preserve the origin diagnostic")
                        .to_string(),
                    expected
                );
            }
            for (package, resolved) in [
                ("rca-core", "rca_core"),
                ("rust-composable-architecture", "renamed_rca"),
            ] {
                assert_eq!(
                    super::public_library_target_for_manifest(package, resolved)
                        .expect("unambiguous names do not need manifest context"),
                    resolved
                );
            }
            return;
        }

        for case in [
            "missing",
            "missing-manifest",
            "unrelated",
            "conflicting-origin",
            "reserved-alias",
        ] {
            let mut child = std::process::Command::new(
                std::env::current_exe().expect("the test executable is available"),
            );
            child
                .args([
                    "--exact",
                    "identifier_tests::library_resolution_requires_context_only_for_ambiguous_canonical_names",
                ])
                .env(CASE, case);
            if case == "missing" {
                child.env_remove("CARGO_MANIFEST_DIR");
            } else if case == "missing-manifest" {
                child.env(
                    "CARGO_MANIFEST_DIR",
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/manifest-reading"),
                );
            } else if matches!(case, "conflicting-origin" | "reserved-alias") {
                child.env(
                    "CARGO_MANIFEST_DIR",
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/manifest-reading")
                        .join(case),
                );
            } else {
                child.env("CARGO_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));
            }
            let output = child.output().expect("the isolated context test runs");
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn resolves_canonical_packages_to_their_public_library_targets() {
        let cases = [
            (
                "rust-composable-architecture",
                "rust_composable_architecture",
                CrateOrigin::CanonicalDependency,
                "rca",
            ),
            (
                "rust-composable-architecture",
                "rust_composable_architecture",
                CrateOrigin::ExplicitAlias,
                "rust_composable_architecture",
            ),
            (
                "rust-composable-architecture",
                "renamed_rca",
                CrateOrigin::ExplicitAlias,
                "renamed_rca",
            ),
            (
                "rust-composable-architecture",
                "gen",
                CrateOrigin::ExplicitAlias,
                "gen",
            ),
            (
                "rca-core",
                "rca_core",
                CrateOrigin::CanonicalDependency,
                "rca_core",
            ),
            (
                "rca-core",
                "renamed_core",
                CrateOrigin::ExplicitAlias,
                "renamed_core",
            ),
        ];

        for (package, resolved, origin, expected) in cases {
            assert_eq!(public_library_target(package, resolved, origin), expected);
        }
    }

    #[test]
    fn identifies_self_and_dependency_manifest_origins() {
        let cases = [
            (
                r#"[package]
name = "rust-composable-architecture"
"#,
                CrateOrigin::PackageSelfIntegration,
            ),
            (
                r#"[dependencies]
rust-composable-architecture = "0.8"
"#,
                CrateOrigin::CanonicalDependency,
            ),
            (
                r#"[dependencies]
rust_composable_architecture = { package = "rust-composable-architecture", version = "0.8" }
"#,
                CrateOrigin::ExplicitAlias,
            ),
            (
                r#"[target.'cfg(unix)'.dev-dependencies]
rust_composable_architecture = { package = "rust-composable-architecture", version = "0.8" }
"#,
                CrateOrigin::ExplicitAlias,
            ),
            (
                r"[dependencies]
rust-composable-architecture = { workspace = true }
",
                CrateOrigin::CanonicalDependency,
            ),
        ];

        for (manifest, expected) in cases {
            let manifest = manifest.parse::<DocumentMut>().expect("manifest parses");
            assert_eq!(
                dependency_origin(
                    &manifest,
                    "rust-composable-architecture",
                    "rust_composable_architecture",
                )
                .expect("unambiguous origin"),
                Some(expected)
            );
        }
    }

    #[test]
    fn dependency_tables_preserve_canonical_names_without_inventing_aliases() {
        let cases = [
            (
                r#"[dependencies]
rust-composable-architecture = { version = "0.1" }
unrelated = { workspace = true }
"#,
                Some(CrateOrigin::CanonicalDependency),
            ),
            (
                r"[dependencies]
unrelated = { workspace = true }
",
                None,
            ),
            (
                r#"[dependencies]
rust-composable-architecture = { version = "0.1" }
"#,
                Some(CrateOrigin::CanonicalDependency),
            ),
            (
                r#"[target.'cfg(unix)'.dependencies.rust-composable-architecture]
version = "0.1"
"#,
                Some(CrateOrigin::CanonicalDependency),
            ),
            (
                r#"[target]
ignored = false

[target.'cfg(unix)'.dependencies]
rust-composable-architecture = "0.1"
"#,
                Some(CrateOrigin::CanonicalDependency),
            ),
            (
                r"[target]
ignored = false
",
                None,
            ),
            (
                r#"[dependencies]
rust_composable_architecture = { version = "0.1" }
"#,
                None,
            ),
        ];
        for (source, expected) in cases {
            let manifest = source.parse::<DocumentMut>().expect("manifest parses");
            let origin = dependency_origin(
                &manifest,
                "rust-composable-architecture",
                "rust_composable_architecture",
            )
            .expect("an ordinary table has an unambiguous dependency origin");
            assert_eq!(origin, expected, "{source}");
            assert_eq!(
                origin.map(|origin| public_library_target(
                    "rust-composable-architecture",
                    "rust_composable_architecture",
                    origin
                )),
                expected.map(|_| "rca"),
                "{source}"
            );
        }
    }

    #[test]
    fn ignores_unrelated_package_entries_and_rejects_conflicting_origins() {
        let unrelated = r#"[dependencies]
rust_composable_architecture = { package = "unrelated-package", version = "0.8" }
"#
        .parse::<DocumentMut>()
        .expect("manifest parses");
        assert_eq!(
            dependency_origin(
                &unrelated,
                "rust-composable-architecture",
                "rust_composable_architecture",
            )
            .expect("unrelated package is ignored"),
            None
        );

        let conflicting = r#"[dependencies]
rust-composable-architecture = "0.8"

[target.'cfg(unix)'.dependencies]
rust_composable_architecture = { package = "rust-composable-architecture", version = "0.8" }
"#
        .parse::<DocumentMut>()
        .expect("manifest parses");
        assert_eq!(
            dependency_origin(
                &conflicting,
                "rust-composable-architecture",
                "rust_composable_architecture",
            )
            .expect_err("conflicting origins fail")
            .to_string(),
            "RCA macros found conflicting canonical and aliased Cargo dependency origins for `rust-composable-architecture`"
        );
    }
}

/// Derives `rca::DependencyKey` from typed live and optional test defaults.
///
/// Apply `#[dependency(value = Type, live = expression)]` to a unit struct.
/// Add `test = expression` for a reusable test default. Without it, missing
/// test dependencies retain the trait's failure behaviour rather than using live values.
///
/// # Examples
///
/// ```
/// use rca::DependencyKey as _;
/// use rca_macros::DependencyKey;
///
/// #[derive(DependencyKey)]
/// #[dependency(value = u32, live = 7, test = 3)]
/// struct Count;
/// assert_eq!(Count::live_value(), 7);
/// assert_eq!(Count::test_value(), 3);
/// ```
#[proc_macro_derive(DependencyKey, attributes(dependency))]
pub fn derive_dependency_key(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_dependency_key(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives explicit test configuration for named endpoint clients.
///
/// Endpoints must be `Arc<dyn Fn(Args...) -> Return + Send + Sync>`, using `Arc`
/// or its `std::sync::Arc` path. Field-level `#[dependency(test = expression)]`
/// supplies explicit endpoint defaults. Ordinary members require
/// `#[dependency(member = expression)]`; no implicit `Default` bound is added.
/// The options are mutually exclusive. Other endpoints panic on invocation with type
/// and field context, including endpoints returning futures. The diagnostic also
/// includes the `test_value()` construction call site, not the invocation site
/// (which cannot be recovered through `dyn Fn`). Borrowed arguments and results,
/// higher-ranked signatures, output aliases and generic clients retain their normal
/// Rust semantics. Whole-field aliases are unsupported because the derive needs
/// the endpoint's argument count. Rustc checks lifetimes and constant expressions.
/// Override endpoints using struct update
/// syntax. This derive supplies neither production values nor `Default`,
/// `Debug`, `Clone`, setters or trait forwarding.
/// Existing concrete clients, static clients and client traits remain supported
/// without this derive. It does not alter dependency lookup precedence.
///
/// Return types are passed through unchanged: synchronous `Result<T, E>` and
/// futures yielding typed results need no special macro syntax. The derive
/// neither polls nor spawns futures, erases errors, nor adds `'static`, `Clone`,
/// `Debug` or `Send` bounds to arguments or results. Closure captures must satisfy
/// the declared `Send + Sync` storage bounds. Signature checks are syntactic, not type resolution:
/// aliases within inputs and outputs remain rustc's responsibility. Endpoints may
/// use unqualified, `std` or `core` paths for `Fn`, `Send` and `Sync`, and an optional
/// object lifetime. Explicit test expressions still validate this grammar;
/// ordinary members are initialised verbatim and are not treated as endpoints.
/// Explicit defaults may compose `rca_core::EndpointStub` with `test-util`;
/// the derive installs no issue reporter, fallback, or production implementation.
///
/// # Examples
///
/// Construct production endpoints explicitly. Select the generated configuration
/// through an existing dependency key's test expression, then override only the
/// endpoints a test uses. Implement `Debug` manually to redact captured resources.
///
/// ```
/// use std::{fmt, sync::Arc};
/// use rca::DependencyKey as _;
/// use rca_macros::{DependencyClient, DependencyKey};
///
/// #[derive(Clone, DependencyClient)]
/// struct Client {
///     #[dependency(member = String::from("test client"))]
///     label: String,
///     fetch: Arc<dyn Fn(String) -> String + Send + Sync>,
///     #[dependency(test = Arc::new(|| 3))]
///     count: Arc<dyn Fn() -> usize + Send + Sync>,
/// }
///
/// impl Client {
///     fn live() -> Self {
///         Self {
///             label: String::from("live client"),
///             fetch: Arc::new(|name| format!("live:{name}")),
///             count: Arc::new(|| 10),
///         }
///     }
/// }
///
/// impl fmt::Debug for Client {
///     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
///         f.debug_struct("Client")
///             .field("endpoints", &"<redacted>")
///             .finish()
///     }
/// }
///
/// #[derive(DependencyKey)]
/// #[dependency(value = Client, live = Client::live(), test = Client::test_value())]
/// struct ClientKey;
///
/// let test = Client {
///     fetch: Arc::new(|name| format!("test:{name}")),
///     ..ClientKey::test_value()
/// };
/// assert_eq!((test.fetch)(String::from("item")), "test:item");
/// assert_eq!((test.count)(), 3);
/// assert_eq!((ClientKey::live_value().count)(), 10);
/// assert_eq!(format!("{test:?}"), "Client { endpoints: \"<redacted>\" }");
/// ```
///
/// # Panics
///
/// Calling an unannotated endpoint from `test_value()` panics immediately,
/// identifying the concrete client type and field. For async endpoints, the
/// panic occurs at invocation, not when the returned future is polled.
#[proc_macro_derive(DependencyClient, attributes(dependency))]
pub fn derive_dependency_client(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    dependency_client::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives borrowing state-path constructors for named state fields and cases.
///
/// Annotate a field with `#[rca(optional)]`, `#[rca(keyed)]`, or
/// `#[rca(skip)]` when its default required projection is not appropriate.
/// Optional and keyed fields must be `Option<T>` and `rca::KeyedState<Id, T>`
/// respectively; concrete and generic type aliases are supported.
/// On enums, place helper attributes on variants, not their payload fields.
/// Unit cases return `Some(())` when matched. Cases with multiple fields use
/// `AggregateStatePath`: `get` returns a tuple of shared field references,
/// `get_mut` returns disjoint mutable references, and `embed` consumes an owned
/// tuple. Named fields follow declaration order; fields stay in their original
/// enum storage. Single-field cases retain `CaseStatePath`.
/// Case-derived method names use snake case, but retain the original spelling
/// if word conversion would lose a valid identifier such as `__` or `__1`.
///
/// ```
/// use rca_macros::StatePaths;
///
/// #[derive(StatePaths)]
/// enum State<'a> {
///     Editing { text: String, label: &'a str },
///     Idle,
/// }
///
/// let label = String::from("local");
/// let path = StatePaths::editing();
/// let mut state = path.embed((String::from("draft"), label.as_str()));
/// let (text, label) = path.get_mut(&mut state).unwrap();
/// text.push_str(" saved");
/// *label = "saved";
/// assert_eq!(path.get(&state).map(|(text, label)| (text.as_str(), *label)),
///            Some(("draft saved", "saved")));
/// assert!(StatePaths::idle().get(&state).is_none());
/// ```
/// Namespaces with type parameters are uninhabited type aliases without
/// `Debug`; call their associated path methods directly. The prefix
/// `__rca_path_namespace_` is reserved for generated sibling storage modules.
#[proc_macro_derive(StatePaths, attributes(rca))]
pub fn derive_state_paths(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    state_paths::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Derives borrowing action-path constructors and owned action embedders.
///
/// Annotate enum variants with `#[rca(keyed)]` or `#[rca(skip)]` where needed.
/// Unit cases use `AggregateActionPath` and match as `Some(())`. Multi-field
/// cases use the same path type: extraction returns a tuple of references to
/// the original fields, and embedding consumes an owned tuple. Named fields
/// follow declaration order. Single-field cases retain `ActionPath`, and
/// `#[rca(keyed)]` requires exactly one payload field.
/// Method names use snake case, retaining source spellings such as `__` and
/// `__1` when word conversion would not produce a valid identifier.
///
/// ```
/// use rca_macros::ActionPaths;
///
/// #[derive(ActionPaths)]
/// enum Action { Save(String, usize), Idle }
///
/// let path = ActionPaths::save();
/// let action = path.embed((String::from("owned"), 3));
/// let (text, revision) = path.extract(&action).unwrap();
/// assert_eq!((text.as_str(), *revision), ("owned", 3));
/// assert_eq!(ActionPaths::idle().extract(&Action::Idle), Some(()));
/// ```
/// Namespaces with type parameters are uninhabited type aliases without
/// `Debug`; call their associated path methods directly. The prefix
/// `__rca_path_namespace_` is reserved for generated sibling storage modules.
#[proc_macro_derive(ActionPaths, attributes(rca))]
pub fn derive_action_paths(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    action_paths::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Combines one or more reducer expressions in source order.
///
/// Equivalent to `rca::combine((first, second, ...))`, including a one-element
/// tuple. Each expression is evaluated once; the resulting reducers keep their
/// concrete types and run in declaration order. Domains must match as required
/// by `combine`. An empty list is rejected rather than creating a no-op reducer.
///
/// # Examples
///
/// ```
/// use rca::{Effect, Reduce, Reducer};
/// use rca_macros::reducers;
///
/// let first = Reduce::new(|state: &mut usize, action: &usize, _| {
///     *state += action;
///     Effect::none()
/// });
/// let second = Reduce::new(|state: &mut usize, _: &usize, _| {
///     *state *= 2;
///     Effect::none()
/// });
/// let combined = reducers![first, second];
/// let _: &dyn Reducer<State = usize, Action = usize> = &combined;
/// ```
#[proc_macro]
pub fn reducers(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as reducers::ReducersInput);
    reducers::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Resolves generated references through the facade first, then direct core.
///
/// Self-use expands to `crate`; external use retains Cargo's dependency alias.
/// Returns a call-site diagnostic if neither package can be resolved, or if the
/// facade's ambiguous public library name cannot be validated against its manifest.
pub(crate) fn rca_path() -> syn::Result<proc_macro2::TokenStream> {
    for package in ["rust-composable-architecture", "rca-core"] {
        match crate_name(package) {
            Ok(FoundCrate::Itself) => return Ok(quote!(crate)),
            Ok(FoundCrate::Name(name)) => {
                let public_target = public_library_target_for_manifest(package, &name)?;
                let ident = rust_ident(&public_target, proc_macro2::Span::call_site())?;
                return Ok(quote!(::#ident));
            }
            Err(_error) => {}
        }
    }

    Err(syn::Error::new(
        proc_macro2::Span::call_site(),
        "RCA macros require `rust-composable-architecture` or `rca-core` in Cargo dependencies",
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Distinguishes the facade's public library name from a caller-chosen alias.
enum CrateOrigin {
    /// An integration target belongs to the requested package itself.
    PackageSelfIntegration,
    /// The dependency key uses the package's hyphenated canonical name.
    CanonicalDependency,
    /// The dependency key deliberately renames the package, even to its underscored name.
    ExplicitAlias,
}

/// Maps canonical facade use to `rca` without rewriting an explicit alias.
fn public_library_target<'a>(
    package: &str,
    resolved_name: &'a str,
    origin: CrateOrigin,
) -> &'a str {
    if package == "rust-composable-architecture"
        && matches!(
            origin,
            CrateOrigin::PackageSelfIntegration | CrateOrigin::CanonicalDependency
        )
    {
        "rca"
    } else {
        resolved_name
    }
}

/// Consults the manifest only when the resolved facade name could be an alias.
///
/// Returns a diagnostic for missing, unreadable or ambiguous manifest context.
fn public_library_target_for_manifest(package: &str, resolved_name: &str) -> syn::Result<String> {
    if package != "rust-composable-architecture" || resolved_name != "rust_composable_architecture"
    {
        return Ok(resolved_name.to_owned());
    }

    let manifest_path = manifest_path()?;
    let manifest = read_manifest(&manifest_path)?;
    let origin = dependency_origin(&manifest, package, resolved_name)?.ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "RCA macros could not determine the Cargo dependency origin for `rust-composable-architecture`",
        )
    })?;

    Ok(public_library_target(package, resolved_name, origin).to_owned())
}

/// Locates the invoking package's manifest, or reports missing Cargo context.
fn manifest_path() -> syn::Result<PathBuf> {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "RCA macros require Cargo manifest context",
        )
    })?;
    Ok(PathBuf::from(manifest_dir).join("Cargo.toml"))
}

/// Reads external manifest input and reports I/O or TOML failures at the derive call.
fn read_manifest(path: &Path) -> syn::Result<DocumentMut> {
    let source = fs::read_to_string(path).map_err(|_error| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "RCA macros could not read the active Cargo manifest",
        )
    })?;
    source.parse::<DocumentMut>().map_err(|_error| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "RCA macros could not parse the active Cargo manifest",
        )
    })
}

/// Resolves a matching dependency key across ordinary and target-specific tables.
///
/// Package self-use takes precedence. Conflicting canonical and alias matches
/// produce a diagnostic rather than selecting whichever table happens to come first.
fn dependency_origin(
    manifest: &DocumentMut,
    package: &str,
    resolved_name: &str,
) -> syn::Result<Option<CrateOrigin>> {
    if manifest
        .get("package")
        .and_then(Item::as_table_like)
        .and_then(|package_table| package_table.get("name"))
        .and_then(Item::as_str)
        == Some(package)
    {
        return Ok(Some(CrateOrigin::PackageSelfIntegration));
    }

    let mut found_canonical = false;
    let mut found_alias = false;
    for dependencies in dependency_tables(manifest) {
        for (name, dependency) in dependencies.iter() {
            if sanitize_crate_name(name) != resolved_name
                || !dependency_declares_requested_package(name, dependency, package)
            {
                continue;
            }
            if name == package {
                found_canonical = true;
            } else {
                found_alias = true;
            }
        }
    }

    match (found_canonical, found_alias) {
        (true, true) => Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            format!(
                "RCA macros found conflicting canonical and aliased Cargo dependency origins for `{package}`"
            ),
        )),
        (true, false) => Ok(Some(CrateOrigin::CanonicalDependency)),
        (false, true) => Ok(Some(CrateOrigin::ExplicitAlias)),
        (false, false) => Ok(None),
    }
}

/// Checks local package metadata after Cargo has already resolved the crate name.
///
/// An inherited workspace declaration needs no local package field: the upstream
/// resolver has established that its matching key refers to the requested package.
fn dependency_declares_requested_package(name: &str, dependency: &Item, package: &str) -> bool {
    let Some(fields) = dependency.as_table_like() else {
        return name == package;
    };

    if let Some(explicit_package) = fields.get("package").and_then(Item::as_str) {
        return explicit_package == package;
    }

    fields
        .get("workspace")
        .and_then(Item::as_bool)
        .is_some_and(|workspace| workspace)
        || name == package
}

/// Borrows dependency tables visible to ordinary, integration and target-specific code.
///
/// Build dependencies are excluded because generated library code cannot name them.
fn dependency_tables(document: &DocumentMut) -> Vec<&dyn TableLike> {
    let mut tables = Vec::new();
    push_dependency_tables(document.as_table(), &mut tables);

    if let Some(targets) = document.get("target").and_then(Item::as_table_like) {
        for (_, target) in targets.iter() {
            if let Some(target) = target.as_table_like() {
                push_dependency_tables(target, &mut tables);
            }
        }
    }

    tables
}

/// Appends runtime and test dependencies from one package or target table.
fn push_dependency_tables<'a>(table: &'a dyn TableLike, tables: &mut Vec<&'a dyn TableLike>) {
    for name in ["dependencies", "dev-dependencies"] {
        if let Some(dependencies) = table.get(name).and_then(Item::as_table_like) {
            tables.push(dependencies);
        }
    }
}

/// Applies Cargo's hyphen-to-underscore crate-name spelling.
fn sanitize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Selects whether a generated Rust name needs keyword escaping.
enum IdentifierClass {
    /// Ordinary identifiers, including contextual keywords valid in this position.
    Plain,
    /// A reserved keyword whose raw spelling is a valid identifier.
    RawRequired,
    /// A path keyword for which Rust forbids raw spelling.
    Forbidden,
}

/// Builds an identifier at the source span, escaping keywords where Rust permits it.
///
/// Generated names and compiler-recovery input are validated before use. Invalid
/// spelling and forbidden path keywords produce diagnostics instead of panics.
pub(crate) fn rust_ident(name: &str, span: proc_macro2::Span) -> syn::Result<syn::Ident> {
    let name = name.trim_start_matches("r#");
    let parsed = match classify_identifier(name) {
        IdentifierClass::Plain => syn::parse_str::<syn::Ident>(name),
        IdentifierClass::RawRequired => syn::parse_str::<syn::Ident>(&format!("r#{name}")),
        IdentifierClass::Forbidden => {
            return Err(syn::Error::new(
                span,
                format!(
                    "RCA derives cannot generate identifier `{name}` because it is not a legal raw Rust identifier"
                ),
            ));
        }
    };
    let mut ident = parsed.map_err(|_error| {
        syn::Error::new(
            span,
            format!("RCA derives cannot generate identifier `{name}` because it is not a legal Rust identifier"),
        )
    })?;
    ident.set_span(span);
    Ok(ident)
}

/// Removes raw syntax for comparisons and generated-name composition.
pub(crate) fn canonical_ident(ident: &syn::Ident) -> String {
    let mut name = ident.to_string();
    if name.starts_with("r#") {
        name.replace_range(..2, "");
    }
    name
}

/// Shields generated type references without rewriting caller-owned generic syntax.
///
/// Fresh alias parameters leave caller bounds, payload macros and value paths
/// untouched. Type aliases avoid import ambiguity with crate names, including
/// function-local declarations. Target parameters can differ from emitted scopes.
pub(crate) fn type_reference(
    input: &impl quote::ToTokens,
    generics: &syn::Generics,
    target_generics: &syn::Generics,
    name: &syn::Ident,
    context: &str,
) -> (syn::Ident, proc_macro2::TokenStream) {
    let canonical = canonical_ident(name);
    if !generics
        .type_params()
        .any(|parameter| canonical_ident(&parameter.ident) == canonical)
    {
        return (name.clone(), proc_macro2::TokenStream::new());
    }
    let alias = state_paths::fresh_binding(input, &format!("__Rca{context}{canonical}"));
    let mut parameters = syn::Generics::default();
    for (index, parameter) in target_generics.params.iter().enumerate() {
        parameters.params.push(match parameter {
            syn::GenericParam::Lifetime(parameter) => {
                let lifetime = &parameter.lifetime;
                syn::parse_quote!(#lifetime)
            }
            syn::GenericParam::Type(_) => {
                let ident = state_paths::fresh_binding(
                    input,
                    &format!("__Rca{context}{canonical}Type{index}"),
                );
                syn::parse_quote!(#ident)
            }
            syn::GenericParam::Const(parameter) => {
                let ident = &parameter.ident;
                let ty = &parameter.ty;
                syn::parse_quote!(const #ident: #ty)
            }
        });
    }
    let arguments = state_paths::type_arguments(&parameters);
    let declaration = quote! {
        #[doc(hidden)]
        type #alias #parameters = #name #arguments;
    };
    (alias, declaration)
}

/// Converts case names to methods without discarding valid identifier-only spellings.
///
/// Word conversion can erase underscores or leave a leading digit. Such names
/// retain their source spelling; forbidden path keywords still receive diagnostics.
pub(crate) fn snake_case_ident(ident: &syn::Ident) -> syn::Result<syn::Ident> {
    let name = canonical_ident(ident);
    let snake = name.to_snake_case();
    match rust_ident(&snake, ident.span()) {
        Err(_) if classify_identifier(&snake) != IdentifierClass::Forbidden => {
            rust_ident(&name, ident.span())
        }
        result => result,
    }
}

/// Matches one unqualified path segment, treating raw and ordinary names as equivalent.
pub(crate) fn path_is_ident(path: &syn::Path, name: &str) -> bool {
    path.get_ident()
        .is_some_and(|ident| ident == name || ident.to_string().strip_prefix("r#") == Some(name))
}

/// Classifies reserved words without rejecting contextual words such as `union`.
fn classify_identifier(name: &str) -> IdentifierClass {
    if matches!(name, "crate" | "self" | "Self" | "super") {
        return IdentifierClass::Forbidden;
    }

    if matches!(
        name,
        "abstract"
            | "as"
            | "async"
            | "await"
            | "become"
            | "box"
            | "break"
            | "const"
            | "continue"
            | "do"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "final"
            | "fn"
            | "for"
            | "gen"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "macro"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "override"
            | "priv"
            | "pub"
            | "ref"
            | "return"
            | "static"
            | "struct"
            | "trait"
            | "true"
            | "try"
            | "type"
            | "typeof"
            | "unsafe"
            | "unsized"
            | "use"
            | "virtual"
            | "where"
            | "while"
            | "yield"
    ) {
        IdentifierClass::RawRequired
    } else {
        IdentifierClass::Plain
    }
}

/// Emits the dependency trait implementation while leaving defaults caller-owned.
///
/// Shape errors precede attribute errors. Duplicate attributes precede option
/// parsing, and duplicate options precede parsing their replacement expressions.
fn expand_dependency_key(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    match &input.data {
        Data::Struct(data) if matches!(data.fields, Fields::Unit) => {}
        Data::Struct(data) => {
            return Err(syn::Error::new_spanned(
                &data.fields,
                "DependencyKey can only be derived for a unit struct",
            ));
        }
        Data::Enum(data) => {
            return Err(syn::Error::new_spanned(
                data.enum_token,
                "DependencyKey can only be derived for a unit struct",
            ));
        }
        Data::Union(data) => {
            return Err(syn::Error::new_spanned(
                data.union_token,
                "DependencyKey can only be derived for a unit struct",
            ));
        }
    }

    let mut dependency_attribute = None;
    for attribute in &input.attrs {
        if path_is_ident(attribute.path(), "dependency")
            && dependency_attribute.replace(attribute).is_some()
        {
            return Err(syn::Error::new_spanned(
                attribute,
                "duplicate `dependency` attribute",
            ));
        }
    }
    let attribute = dependency_attribute.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "missing `#[dependency(value = Type, live = expression)]` attribute",
        )
    })?;

    let mut value = None::<Type>;
    let mut live = None::<Expr>;
    let mut test = None::<Expr>;
    attribute.parse_nested_meta(|meta| {
        if path_is_ident(&meta.path, "value") {
            if value.is_some() {
                return Err(meta.error("duplicate `value` option"));
            }
            value = Some(meta.value()?.parse()?);
            return Ok(());
        }
        if path_is_ident(&meta.path, "live") {
            if live.is_some() {
                return Err(meta.error("duplicate `live` option"));
            }
            live = Some(meta.value()?.parse()?);
            return Ok(());
        }
        if path_is_ident(&meta.path, "test") {
            if test.is_some() {
                return Err(meta.error("duplicate `test` option"));
            }
            test = Some(meta.value()?.parse()?);
            return Ok(());
        }
        Err(meta.error("unknown dependency option; expected `value`, `live` or `test`"))
    })?;

    let value = value
        .ok_or_else(|| syn::Error::new_spanned(attribute, "missing `value` dependency option"))?;
    let live =
        live.ok_or_else(|| syn::Error::new_spanned(attribute, "missing `live` dependency option"))?;
    let ident = &input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();
    let rca = rca_path()?;

    let test_method = test.map(|test| {
        quote! {
            fn test_value() -> Self::Value {
                #test
            }
        }
    });

    Ok(quote! {
        impl #impl_generics #rca::DependencyKey for #ident #type_generics #where_clause {
            type Value = #value;

            fn live_value() -> Self::Value {
                #live
            }

            #test_method
        }
    })
}
