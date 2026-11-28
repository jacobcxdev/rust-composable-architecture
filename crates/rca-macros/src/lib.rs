#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

//! Macros for RCA reducers, typed paths, dependencies and test snapshots.
//!
//! The examples use `rca` with its `macros` feature. For example, `use rca::Snapshot;`
//! imports both the derive and the trait that provides the `snapshot` method.
//! Import a macro from the facade whose types it should use. This also works when Cargo
//! renames that dependency. Direct `rca-macros` imports target a declared `rca-core`
//! dependency; if there is no core dependency, they can use a single facade.
//! Use `StatePaths` and `ActionPaths` to select fields and enum variants without copying their values.
//! Use `reducers!` to run several reducers in order. Use `#[reducer]` to declare child composition.
//! The macros generate Rust code at compile time. They do not start a runtime.

use heck::ToSnakeCase;
use proc_macro::TokenStream;
use proc_macro_crate::{FoundCrate, crate_name};
use quote::quote;
use syn::{Data, DeriveInput, Expr, Fields, Type, parse_macro_input};

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
/// The generated snapshot supports cloning, equality and debug output when its fields do.
/// Use `#[snapshot(with = function, value = OwnedType)]` to capture a field yourself.
/// Use `#[snapshot(omit = "reason")]` to exclude it. Redaction changes debug output, not equality.
/// `#[snapshot(name = History)]` names the generated type.
/// With `into = ExistingHistory`, the derive uses your existing named-field struct and leaves its traits unchanged.
///
/// Omitted tuple fields compact output indices. Source layout, discriminants and
/// unrelated helper attributes do not carry over. Capture reads fields in declaration
/// order, not atomically. A custom capture must keep later source edits out of the snapshot.
/// Edits to expected history must leave the source and other copies unchanged.
/// The derive cannot prove this independence.
/// Recursive fields require finite custom capture or a manual implementation.
/// A custom function pointer owns no captured environment. Its behavioural stability
/// and equality policy remain the caller's responsibility, as with any custom value.
///
/// # Examples
///
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::Snapshot; }
/// use rca::Snapshot;
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
/// A generated domain can need a private type alias if a type parameter hides its name.
/// Avoid sibling type names that start with `__RcaReducerDomain`. Generated paths
/// also use the private names described by the `StatePaths` and `ActionPaths` derives.
///
/// # Examples
///
/// This counter has one required child. The macro generates the parent's state
/// field and action variant from the child's types. Without the macro, write
/// those types by hand and call `rca::scope` with their state and action paths.
/// In an application with the `macros` feature, import `reducer` from `rca`.
/// To run the example with RCA's test host, also enable `test-util` and add `futures-executor` as a dev-dependency.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::reducer; pub use rca_test::DeterministicRuntime; }
/// use rca::{Effect, ReduceContext, Reducer, Runtime, Store, reducer};
///
/// struct Counter;
/// impl Reducer for Counter {
///     type State = usize;
///     type Action = ();
///     fn reduce(&mut self, count: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
///         *count += 1;
///         Effect::none()
///     }
/// }
///
/// #[reducer(
///     state(generated(struct State { #[reducer(child = counter)] count: _ })),
///     action(generated(enum Action { #[reducer(child = counter)] Increment(_) })),
///     body(generated),
///     child(counter, reducer = Counter, kind = required,
///         state = StatePaths::count(), action = ActionPaths::increment(),
///         bind(fn bind_counter(child: Counter)),
///     ),
/// )]
/// struct App<C> {
///     #[reducer(child = counter)]
///     counter: C,
/// }
///
/// async fn increment(runtime: Runtime) -> Result<usize, Box<dyn std::error::Error>> {
///     let app = App { counter: bind_counter(Counter) };
///     let store = Store::new(State { count: 0 }, app, runtime);
///     store.send(Action::Increment(())).await?.finish().await;
///     Ok(store.shutdown_into_state().await?.count)
/// }
/// # fn main() {
/// # let host = rca::DeterministicRuntime::new();
/// # let result = futures_executor::block_on(host.run(increment(host.runtime()))).unwrap();
/// # assert_eq!(result.unwrap(), 1);
/// # }
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
/// If a type parameter hides a source or generated type name, the derive creates
/// a private alias beside the enum. Avoid sibling type names that start with `__RcaCaseDescriptors`.
///
/// # Examples
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::CaseDescriptors; }
/// use rca::CaseDescriptors;
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

    use super::{IdentifierClass, classify_identifier, rust_ident};

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

    /// Checks the generated path or diagnostic inside an isolated consumer environment.
    fn check_manifest_context(case: &str) {
        let root = match case {
            "private-short" => super::MacroRoot::Long,
            "optional-core" | "short-unavailable" => super::MacroRoot::Short,
            _ => super::MacroRoot::Standalone,
        };
        let path = root.path();
        match case {
            "canonical" | "optional-core" => assert_eq!(path.unwrap().to_string(), ":: rca"),
            "optional-short" => assert_eq!(path.unwrap().to_string(), ":: domain_core"),
            "private-short" | "standalone-private" | "standalone-private-long" => {
                assert_eq!(path.unwrap().to_string(), ":: architecture");
            }
            "standalone-long" => assert_eq!(path.unwrap().to_string(), ":: r#type"),
            "both-facades" => assert!(
                path.unwrap_err()
                    .to_string()
                    .starts_with("both RCA facades are declared")
            ),
            "short-unavailable" => {
                let diagnostic =
                    "RCA macros re-exported by `rca` require that package as a direct dependency";
                assert_eq!(path.unwrap_err().to_string(), diagnostic);
                let error = super::reducer::expand_from(
                    quote::quote!(
                        state(supplied(State)),
                        action(supplied(Action)),
                        body(supplied(body))
                    ),
                    quote::quote!(
                        struct Feature<R> {
                            body: R,
                        }
                    ),
                    root,
                )
                .expect_err("a valid reducer must not expand against an unavailable facade");
                assert_eq!(error.to_string(), diagnostic);
            }
            "facade-with-core" => assert_eq!(path.unwrap().to_string(), ":: rca_core"),
            "self-core" => assert_eq!(path.unwrap().to_string(), "crate"),
            "missing" => assert_eq!(
                path.unwrap_err().to_string(),
                "RCA macros require `rca`, `rust-composable-architecture` or `rca-core` in Cargo dependencies",
            ),
            "reserved-alias" => assert_eq!(
                path.unwrap_err().to_string(),
                "RCA derives cannot generate identifier `self` because it is not a legal raw Rust identifier",
            ),
            _ => panic!("unknown manifest context case"),
        }
    }

    #[test]
    fn resolves_crate_paths_from_cargo_context() {
        const CASE: &str = "RCA_MACROS_MANIFEST_CONTEXT_CASE";
        if let Ok(case) = std::env::var(CASE) {
            check_manifest_context(&case);
            return;
        }

        for case in [
            "canonical",
            "optional-short",
            "optional-core",
            "private-short",
            "standalone-private",
            "standalone-private-long",
            "standalone-long",
            "both-facades",
            "short-unavailable",
            "facade-with-core",
            "self-core",
            "missing",
            "reserved-alias",
        ] {
            let mut child = std::process::Command::new(
                std::env::current_exe().expect("the test executable is available"),
            );
            child
                .args([
                    "--exact",
                    "identifier_tests::resolves_crate_paths_from_cargo_context",
                ])
                .env(CASE, case);
            if case == "missing" {
                child.env_remove("CARGO_MANIFEST_DIR");
            } else {
                let fixtures =
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
                let manifest = match case {
                    // This consumer has the facade's package name and direct core dependency.
                    "facade-with-core" => fixtures.join("crate-self-state-paths"),
                    "short-unavailable" => fixtures.join("manifest-reading/standalone-long"),
                    "standalone-private" => {
                        child.env("CARGO_PKG_NAME", "rca");
                        fixtures.join("manifest-reading/private-short")
                    }
                    "standalone-private-long" => {
                        child.env("CARGO_PKG_NAME", "rust-composable-architecture");
                        fixtures.join("manifest-reading/private-long")
                    }
                    _ => fixtures.join("manifest-reading").join(case),
                };
                child.env("CARGO_MANIFEST_DIR", manifest);
            }
            if matches!(case, "private-short" | "facade-with-core" | "self-core") {
                // Cargo sets this for integration targets, which are not self-use.
                child.env_remove("CARGO_TARGET_TMPDIR");
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
}

/// Derives `rca::DependencyKey` from typed live and optional test defaults.
///
/// Apply `#[dependency(value = Type, live = expression)]` to a unit struct.
/// Add `test = expression` for a reusable test default. Without it, missing
/// test dependencies retain the trait's failure behaviour rather than using live values.
///
/// # Examples
///
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::DependencyKey; }
/// use rca::DependencyKey;
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
/// Declare each endpoint as `Arc<dyn Fn(Args...) -> Return + Send + Sync>`.
/// Use `Arc` or `std::sync::Arc`. For `Fn`, `Send` and `Sync`, the derive accepts
/// unqualified names and standard `std` or `core` paths. It also accepts an explicit object lifetime.
/// The derive reads the argument count from the field type, so it cannot use an alias for the entire field.
///
/// On an endpoint, `#[dependency(test = expression)]` supplies its test value.
/// The endpoint must still use the same type syntax. On any other field,
/// `#[dependency(member = expression)]` supplies the value directly, without a `Default` bound.
/// A field cannot use both attributes.
///
/// Without a test value, an endpoint panics at the call, even if its return type is a future.
/// The panic names the client type, the field and the place that constructed the client with `test_value()`.
/// It does not identify the endpoint call site. A test can replace an endpoint with struct update syntax.
///
/// The derive preserves return types, including `Result<T, E>` and futures that return typed results.
/// It does not poll or spawn futures. Rust checks borrowed inputs and outputs, higher-ranked signatures,
/// aliases inside the signature, generic clients, lifetimes and constant expressions.
/// Closure captures must meet the endpoint's `Send + Sync` bounds.
/// The derive adds no `'static`, `Clone`, `Debug` or `Send` bounds to arguments or results.
///
/// With `test-util`, explicit defaults can use `rca::EndpointStub`. The derive
/// supplies no issue reporter, fallback, production value, setter or trait implementation
/// for `Default`, `Debug` or `Clone`. It does not forward traits or change dependency lookup order.
/// Concrete clients, static clients and client traits do not require this derive.
///
/// # Examples
///
/// Construct production endpoints explicitly. Select the generated configuration
/// through an existing dependency key's test expression, then override only the
/// endpoints a test uses. Implement `Debug` manually to redact captured resources.
///
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::{DependencyClient, DependencyKey}; }
/// use rca::{DependencyKey, DependencyClient};
/// use std::{fmt, sync::Arc};
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
/// A namespace with type parameters has no values and does not implement `Debug`.
/// Call its path methods directly. Avoid sibling module names that start with `__rca_path_namespace_`.
/// If a type parameter hides a type name, the derive creates an alias with the prefix `__RcaStatePaths`.
/// Avoid sibling type names with this prefix.
///
/// # Examples
///
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::StatePaths; }
/// use rca::StatePaths;
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
/// A namespace with type parameters has no values and does not implement `Debug`.
/// Call its path methods directly. Avoid sibling module names that start with `__rca_path_namespace_`.
/// If a type parameter hides a type name, the derive creates an alias with the prefix `__RcaActionPaths`.
/// Avoid sibling type names with this prefix.
///
/// # Examples
///
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::ActionPaths; }
/// use rca::ActionPaths;
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
/// Enable the `macros` feature on `rca`.
///
/// ```
/// # mod rca { pub use ::rca::*; pub use rca_macros::reducers; }
/// use rca::{Effect, Reduce, Reducer, reducers};
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

/// Defines a derive entry for each facade that emits paths to that facade's types.
///
/// Rust does not pass the path that imported a derive to its implementation.
macro_rules! facade_derive {
    ($canonical:ident($helper:ident), $expand:path; $($root:ident $entry:ident($name:ident)),+) => {
        $(
            #[doc(hidden)]
            #[doc = concat!("Uses the importing facade's types.\n\nEnable the `macros` feature on the facade. See [`", stringify!($canonical), "`](macro@", stringify!($canonical), ") for syntax and examples.")]
            #[proc_macro_derive($name, attributes($helper))]
            pub fn $entry(input: TokenStream) -> TokenStream {
                let input = parse_macro_input!(input as DeriveInput);
                $expand(&input, MacroRoot::$root)
                    .unwrap_or_else(syn::Error::into_compile_error)
                    .into()
            }
        )+
    };
}

facade_derive!(Snapshot(snapshot), snapshot::expand_from;
    Short short_snapshot(__RcaShortSnapshot), Long long_snapshot(__RcaLongSnapshot));
facade_derive!(DependencyKey(dependency), expand_dependency_key_from;
    Short short_dependency_key(__RcaShortDependencyKey), Long long_dependency_key(__RcaLongDependencyKey));
facade_derive!(StatePaths(rca), state_paths::expand_from;
    Short short_state_paths(__RcaShortStatePaths), Long long_state_paths(__RcaLongStatePaths));
facade_derive!(ActionPaths(rca), action_paths::expand_from;
    Short short_action_paths(__RcaShortActionPaths), Long long_action_paths(__RcaLongActionPaths));

/// Exposes the reducer attribute and composition macro through the same facade.
macro_rules! facade_reducers {
    ($root:ident, $attribute:ident, $function:ident) => {
        /// Declares reducer composition. See [`#[reducer]`](macro@reducer) for syntax and examples.
        ///
        /// Enable the `macros` feature on your RCA dependency.
        #[doc(hidden)]
        #[proc_macro_attribute]
        pub fn $attribute(attribute: TokenStream, input: TokenStream) -> TokenStream {
            reducer::expand_from(attribute.into(), input.into(), MacroRoot::$root)
                .unwrap_or_else(syn::Error::into_compile_error)
                .into()
        }

        /// Combines reducers in order. See [`reducers!`] for syntax and examples.
        ///
        /// Enable the `macros` feature on your RCA dependency.
        #[doc(hidden)]
        #[proc_macro]
        pub fn $function(input: TokenStream) -> TokenStream {
            let input = parse_macro_input!(input as reducers::ReducersInput);
            reducers::expand_from(&input, MacroRoot::$root)
                .unwrap_or_else(syn::Error::into_compile_error)
                .into()
        }
    };
}

facade_reducers!(Short, short_reducer, short_reducers);
facade_reducers!(Long, long_reducer, long_reducers);

/// Identifies which public surface supplied a macro that emits RCA types.
#[derive(Clone, Copy)]
pub(crate) enum MacroRoot {
    /// A direct macro import uses the direct core, or a sole facade in older consumers.
    Standalone,
    /// A macro re-exported by the short facade uses that facade's Rust types.
    Short,
    /// A macro re-exported by the long facade uses that facade's Rust types.
    Long,
}

impl MacroRoot {
    /// Returns the generated path, keeping a Cargo rename or a raw identifier.
    pub(crate) fn path(self) -> syn::Result<proc_macro2::TokenStream> {
        match self {
            Self::Short => Self::facade("rca", crate_name("rca")),
            Self::Long => Self::facade(
                "rust-composable-architecture",
                crate_name("rust-composable-architecture"),
            ),
            Self::Standalone => {
                if let Ok(core) = crate_name("rca-core") {
                    return match core {
                        FoundCrate::Itself => Ok(quote!(crate)),
                        FoundCrate::Name(name) => Self::external(&name),
                    };
                }
                match (
                    crate_name("rca"),
                    crate_name("rust-composable-architecture"),
                ) {
                    (short @ Ok(_), Err(_)) => Self::facade("rca", short),
                    (Err(_), long @ Ok(_)) => Self::facade("rust-composable-architecture", long),
                    // The lookup includes the enclosing package, even for its integration
                    // targets. When there is one external facade, keep using that dependency.
                    (Ok(_), long @ Ok(_))
                        if std::env::var("CARGO_PKG_NAME").as_deref() == Ok("rca") =>
                    {
                        Self::facade("rust-composable-architecture", long)
                    }
                    (short @ Ok(_), Ok(_))
                        if std::env::var("CARGO_PKG_NAME").as_deref()
                            == Ok("rust-composable-architecture") =>
                    {
                        Self::facade("rca", short)
                    }
                    (Ok(_), Ok(_)) => Err(syn::Error::new(
                        proc_macro2::Span::call_site(),
                        "both RCA facades are declared; import this macro from the facade whose types it should use",
                    )),
                    (Err(_), Err(_)) => Err(syn::Error::new(
                        proc_macro2::Span::call_site(),
                        "RCA macros require `rca`, `rust-composable-architecture` or `rca-core` in Cargo dependencies",
                    )),
                }
            }
        }
    }

    /// Names the library even from another binary or test target in its package.
    fn facade(
        package: &str,
        found: Result<FoundCrate, proc_macro_crate::Error>,
    ) -> syn::Result<proc_macro2::TokenStream> {
        match found {
            Ok(FoundCrate::Itself) => Self::external(&package.replace('-', "_")),
            Ok(FoundCrate::Name(name)) => Self::external(&name),
            Err(_) => Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                format!(
                    "RCA macros re-exported by `{package}` require that package as a direct dependency"
                ),
            )),
        }
    }

    /// Uses the compiler-visible Cargo alias, including aliases that are Rust keywords.
    fn external(name: &str) -> syn::Result<proc_macro2::TokenStream> {
        let ident = rust_ident(name, proc_macro2::Span::call_site())?;
        Ok(quote!(::#ident))
    }
}

/// Selects whether a generated Rust name needs keyword escaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    expand_dependency_key_from(input, MacroRoot::Standalone)
}

/// Implements the dependency key trait from the same crate as the derive.
fn expand_dependency_key_from(
    input: &DeriveInput,
    root: MacroRoot,
) -> syn::Result<proc_macro2::TokenStream> {
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
    let rca = root.path()?;

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
