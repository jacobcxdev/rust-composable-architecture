//! Exercises reducer expansion through compiled consumers and observable Store behaviour.
//!
//! Runtime authority, local-order and policy fixtures run through separate consumer manifests.
//! Nested Cargo commands inherit the build job's output streams and process group.
//! The caller owns timeout and cancellation for Cargo and its descendants.

use rca::{Effect, ReduceContext, Reducer};
use rca_test::TestStore;

mod supplied {
    use super::{Effect, ReduceContext, Reducer};

    pub struct State(pub usize);
    pub struct Action(pub usize);
    pub struct Body;

    impl Reducer for Body {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            state.0 += action.0;
            if action.0 == 0 {
                Effect::none()
            } else {
                Effect::send(Action(0))
            }
        }
    }

    #[rca_macros::reducer(
        state(supplied(State)),
        action(supplied(Action)),
        body(supplied(r#body))
    )]
    pub struct Feature<R> {
        pub body: R,
    }
}

#[test]
fn supplied_domains_retain_the_callers_exact_types() {
    fn accepts_supplied_domains<R: Reducer<State = supplied::State, Action = supplied::Action>>(
        _: &R,
    ) {
    }

    let feature = supplied::Feature {
        body: supplied::Body,
    };
    accepts_supplied_domains(&feature);
    futures_executor::block_on(async {
        let mut store = TestStore::projected(supplied::State(7), feature, |state| state.0);
        let _sent = store
            .send_with(supplied::Action(3), |state| *state = 10)
            .await
            .unwrap();
        let _received = store.receive(|action| action.0 == 0).await.unwrap();
        store.finish().await.unwrap();
    });
}

mod generated {
    use super::{Effect, ReduceContext, Reducer};

    pub struct Counter;

    impl Reducer for Counter {
        type State = usize;
        type Action = Vec<u8>;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            *state += action.len();
            if action.is_empty() {
                Effect::none()
            } else {
                Effect::send(Vec::new())
            }
        }
    }

    #[rca_macros::reducer(
        state(generated(pub struct State {
            #[reducer(child = counter)]
            pub counter: _,
        })),
        action(generated(pub enum Action {
            #[reducer(child = counter)]
            Counter(_),
        })),
        body(generated),
        child(
            counter,
            reducer = Counter,
            kind = required,
            state = StatePaths::counter(),
            action = ActionPaths::counter(),
            bind(pub fn bind_counter(child: Counter)),
        ),
    )]
    pub struct Feature<C> {
        #[reducer(child = counter)]
        pub counter: C,
    }
}

#[test]
fn generated_child_domains_and_paths_use_associated_payloads() {
    let mut state = generated::State { counter: 7 };
    let state_path = generated::StatePaths::counter();
    let value: &mut <generated::Counter as Reducer>::State = state_path.get_mut(&mut state);
    *value = 11;
    assert_eq!(state.counter, 11);

    let path = generated::ActionPaths::counter();
    let action = path.embed(vec![2, 3]);
    let borrowed: &<generated::Counter as Reducer>::Action = path.extract(&action).unwrap();
    let generated::Action::Counter(direct) = &action;
    assert!(std::ptr::eq(borrowed, direct));
    assert_eq!(borrowed, &[2, 3]);

    let feature = generated::Feature {
        counter: generated::bind_counter(generated::Counter),
    };
    let _: &dyn Reducer<State = generated::State, Action = generated::Action> = &feature;
    futures_executor::block_on(async {
        let mut store = TestStore::projected(state, feature, |state| state.counter);
        let _sent = store.send_with(action, |state| *state = 13).await.unwrap();
        let _received = store
            .receive(|action| {
                let generated::Action::Counter(value) = action;
                value.is_empty()
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

mod ordered {
    use super::{Effect, ReduceContext, Reducer};

    pub struct Counter {
        pub label: u8,
        pub calls: usize,
    }

    impl Reducer for Counter {
        type State = Vec<(u8, usize)>;
        type Action = ();

        fn reduce(
            &mut self,
            state: &mut Self::State,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            self.calls += 1;
            state.push((self.label, self.calls));
            Effect::none()
        }
    }

    #[rca_macros::reducer(
        state(supplied(Vec<(u8, usize)>)), action(supplied(())), body(generated),
    )]
    pub struct Feature {
        #[reducer(reduce)]
        pub first: Counter,
        #[reducer(reduce)]
        pub second: Counter,
    }
}

#[test]
fn generated_body_reduces_persistent_move_only_fields_once_in_declaration_order() {
    let feature = ordered::Feature {
        first: ordered::Counter { label: 1, calls: 0 },
        second: ordered::Counter { label: 2, calls: 0 },
    };
    futures_executor::block_on(async {
        let mut store = TestStore::new(Vec::new(), feature);
        let _first = store
            .send_with((), |state| state.extend([(1, 1), (2, 1)]))
            .await
            .unwrap();
        let _second = store
            .send_with((), |state| state.extend([(1, 2), (2, 2)]))
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

mod unwind_cleanup {
    //! Checks owned effect cleanup when a later generated child reducer panics.

    /// Combines two reducers with the same state and action types.
    #[rca_macros::reducer(
        state(supplied(Vec<u8>)), action(supplied(())), body(generated),
    )]
    pub struct Feature<First, Second> {
        /// Returns an effect that owns a resource before the next reducer runs.
        #[reducer(reduce)]
        pub first: First,
        /// Panics before the generated array can receive its second effect.
        #[reducer(reduce)]
        pub second: Second,
    }
}

#[test]
fn generated_body_drops_an_earlier_effect_when_a_later_reducer_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts disposal without retaining the effect that owns this resource.
    struct Resource(Arc<AtomicUsize>);

    impl Drop for Resource {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let mut resource = Some(Resource(Arc::clone(&drops)));
    let first = rca::Reduce::new(
        move |state: &mut Vec<u8>, (): &(), _: &mut ReduceContext<'_>| {
            state.push(1);
            let resource = resource.take().expect("the first child runs once");
            Effect::future(async move {
                drop(resource);
                None
            })
        },
    );
    let live_drops = Arc::clone(&drops);
    let second = rca::Reduce::new(
        move |state: &mut Vec<u8>, (): &(), _: &mut ReduceContext<'_>| -> Effect<()> {
            state.push(2);
            assert_eq!(live_drops.load(Ordering::Relaxed), 0);
            panic!("second child reduction failed");
        },
    );
    let mut feature = unwind_cleanup::Feature { first, second };
    let cleaned_drops = Arc::clone(&drops);
    let reducer = rca::Reduce::new(
        move |state: &mut Vec<u8>, action: &(), context: &mut ReduceContext<'_>| {
            // Catch inside the caller so Store shutdown cannot mask the original panic.
            let panic = catch_unwind(AssertUnwindSafe(|| feature.reduce(state, action, context)))
                .expect_err("the second child must stop generated reduction");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"second child reduction failed")
            );
            assert_eq!(cleaned_drops.load(Ordering::Relaxed), 1);
            Effect::none()
        },
    );

    futures_executor::block_on(async {
        let mut store = TestStore::new(Vec::new(), reducer);
        let _sent = store
            .send_with((), |state| state.extend([1, 2]))
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn generated_generic_domains_compile_without_warnings() {
    // These existing standalone witnesses also belong to ordinary workspace CI.
    // They preserve lifetime/const generics and move-only child state rather
    // than avoiding generated paths or adding unnecessary Clone bounds.
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/reducer/pass_generics_hygiene.rs");
    tests.pass("tests/ui/reducer/pass_generic_child.rs");
}

#[test]
fn conditional_domains_compile_without_warnings() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/reducer/pass_cfg_boolean_literals.rs");
}

#[test]
fn remaining_generated_domains_compile() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/reducer/pass_cfg_action_alternatives.rs");
    tests.pass("tests/ui/reducer/pass_cfg_child.rs");
    tests.pass("tests/ui/reducer/pass_cfg_generic.rs");
    tests.pass("tests/ui/reducer/pass_const_binding_collisions.rs");
    tests.pass("tests/ui/reducer/pass_effect_map_non_send.rs");
    tests.pass("tests/ui/reducer/pass_empty_mixed.rs");
    tests.pass("tests/ui/reducer/pass_ignored_shapes.rs");
    tests.pass("tests/ui/reducer/pass_local_child_state.rs");
    tests.pass("tests/ui/reducer/pass_local_supplied.rs");
    tests.pass("tests/ui/reducer/pass_selection_options.rs");
}

#[test]
fn invalid_selections_and_borrowing_violations_have_diagnostics() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/reducer/fail_*.rs");
    // These historical names predate the retained mapped-action lifetime boundary.
    tests.compile_fail("tests/ui/reducer/pass_effect_map_borrowed.rs");
    tests.compile_fail("tests/ui/reducer/pass_local_child_action.rs");
}

#[test]
fn generated_runtime_consumers_execute() {
    let manifest_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // Reuse the existing nested-consumer target instead of adding another cache.
    let target = std::path::PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()),
    )
    .join("dependency-client-fixture");

    for (name, binary, selected) in [
        ("d54-authority-runtime", None, false),
        ("d54-local-runtime", None, false),
        ("d54-policy-runtime", None, false),
        ("d54-renamed-core", None, false),
        ("d54-renamed-umbrella", None, false),
        (
            "d54-local-runtime",
            Some("required-child-local-action"),
            false,
        ),
        ("d54-acceptance", Some("cfg-action-alternatives"), true),
        ("d54-acceptance", Some("cfg-generic"), true),
    ] {
        let manifest = manifest_root.join(format!("tests/fixtures/{name}/Cargo.toml"));
        let mut command = std::process::Command::new(env!("CARGO"));
        command
            .arg(if binary.is_some() { "run" } else { "test" })
            .arg("--manifest-path")
            .arg(manifest)
            .arg("--target-dir")
            .arg(&target)
            .env("CARGO_BUILD_JOBS", "1");
        // These two consumers define their runtime tests in the binary target.
        let binary_tests = matches!(name, "d54-renamed-core" | "d54-renamed-umbrella");
        if let Some(binary) = binary {
            command.args(["--bin", binary]);
        } else if binary_tests {
            command.args(["--bin", name]);
        } else {
            command.arg("--lib");
        }
        if selected {
            command.args(["--features", "selected"]);
        }

        let case = binary.unwrap_or(if binary_tests {
            "binary tests"
        } else {
            "library tests"
        });
        let status = command
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|error| {
                panic!("{name} ({case}), selected={selected}: Cargo could not complete: {error}")
            });
        assert!(
            status.success(),
            "{name} ({case}), selected={selected} failed: {status}"
        );
    }
}
