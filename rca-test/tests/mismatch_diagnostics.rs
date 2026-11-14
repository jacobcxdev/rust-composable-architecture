//! Checks bounded mismatch reports, secret redaction and retained assertion failures.

use std::fmt;
use std::fmt::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer};
use rca_test::TestStore;

/// Changes text directly or sends the same change through an effect.
enum Action {
    /// Replaces the state during reduction.
    Set(String),
    /// Produces an action for the receive assertion.
    Start(String),
}

/// Gives send and receive assertions the same text-changing behaviour.
struct Text;

impl Reducer for Text {
    type State = String;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut String,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Set(value) => state.clone_from(value),
            Action::Start(value) => return Effect::send(Action::Set(value.clone())),
        }
        Effect::none()
    }
}

/// Extracts either string panic form used by these diagnostic checks.
///
/// An unexpected payload type fails the test instead of hiding the panic.
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => (*panic.downcast::<&str>().unwrap()).to_owned(),
    }
}

/// Returns the mismatch panic from a real store transition.
///
/// The store drops during that unwind, so teardown cannot replace the first failure.
fn mismatch(expected: &str, actual: &str) -> String {
    panic_message(
        catch_unwind(AssertUnwindSafe(|| {
            let mut store = TestStore::builder(expected.to_owned(), Text)
                .with_snapshot_renderer(Clone::clone)
                .build();
            let _task = block_on(store.send(Action::Set(actual.to_owned()))).unwrap();
        }))
        .unwrap_err(),
    )
}

// Unbounded whole-value dumps or a context window anchored at line one fail this test.
#[test]
fn nested_long_output_shows_local_context_and_bounds_changed_regions() {
    let mut prefix = String::new();
    let mut suffix = String::new();
    for i in 1..=100 {
        writeln!(prefix, "    field_{i}: same,").unwrap();
        writeln!(suffix, "    tail_{i}: same,").unwrap();
    }
    let expected = format!("{prefix}{}{suffix}", "    old: value,\n".repeat(100));
    let actual = format!("{prefix}{}{suffix}", "    new: value,\n".repeat(100));
    let message = mismatch(&expected, &actual);
    assert!(message.contains("98 |     field_98: same,"), "{message}");
    assert!(!message.contains("field_97:"), "{message}");
    assert!(message.contains("101 |     old: value,"), "{message}");
    assert!(message.contains("101 |     new: value,"), "{message}");
    assert!(message.contains("203 |     tail_3: same,"), "{message}");
    assert!(!message.contains("tail_4:"), "{message}");
    assert_eq!(message.matches("old: value,").count(), 16);
    assert_eq!(message.matches("new: value,").count(), 16);
    assert!(message.contains("lines omitted"), "{message}");
    assert!(
        message.contains("Later differences may be omitted."),
        "{message}"
    );
    assert!(message.len() < 15_000);
}

#[test]
fn insertion_deletion_and_boundary_lines_retain_original_numbers() {
    for (expected, actual, want) in [
        ("a\nb\nc", "a\nx\nb\nc", "+ 2 | x"),
        ("a\nx\nb\nc", "a\nb\nc", "- 2 | x"),
        ("old\nb\nc", "new\nb\nc", "+ 1 | new"),
        ("a\nb\nold", "a\nb\nnew", "+ 3 | new"),
        ("", "x", "+ 1 | x"),
        ("x", "", "- 1 | x"),
        ("x", "x\n", "+ 2 | "),
    ] {
        let message = mismatch(expected, actual);
        assert!(message.contains(want), "{message}");
        assert!(message.contains("expected:"), "{message}");
        assert!(message.contains("actual:"), "{message}");
    }
}

#[test]
fn unicode_lines_are_truncated_by_scalar_values_without_invalid_boundaries() {
    let message = mismatch(&"🦀".repeat(1000), &"é".repeat(1000));
    assert!(message.contains("[line truncated]"), "{message}");
    assert!(message.contains("- 1 | 🦀"), "{message}");
    assert!(message.contains("+ 1 | é"), "{message}");
    for line in message.lines().filter(|line| line.contains(" | ")) {
        assert!(line.chars().count() <= 240, "{}", line.chars().count());
    }
    assert!(!message.contains(&"🦀".repeat(241)));
    assert!(!message.contains(&"é".repeat(241)));
}

#[test]
fn identical_renderings_do_not_hide_semantic_inequality() {
    let message = panic_message(
        catch_unwind(AssertUnwindSafe(|| {
            let mut store = TestStore::builder(String::new(), Text)
                .with_snapshot_renderer(|_| "redacted".to_owned())
                .build();
            let _task = block_on(store.send(Action::Set("changed".to_owned()))).unwrap();
        }))
        .unwrap_err(),
    );
    let excerpt = message
        .split_once('\n')
        .unwrap()
        .1
        .split_once("\nruntime:")
        .unwrap()
        .0;
    assert_eq!(
        excerpt,
        "Snapshots differ semantically, but their rendered representations are identical."
    );
}

#[test]
fn equal_snapshots_do_not_invoke_renderer_for_send_or_receive() {
    let mut store = TestStore::builder(String::new(), Text)
        .with_snapshot_renderer(|_| panic!("equal snapshots must not render"))
        .build();
    let _task = block_on(store.send(Action::Start("next".to_owned()))).unwrap();
    let _task = block_on(store.receive_with(|_| true, |s| *s = "next".to_owned())).unwrap();
    block_on(store.finish()).unwrap();
}

/// Keeps a secret beside a value whose change must remain visible.
#[derive(Clone, PartialEq)]
struct Secret {
    /// Sentinel text that no diagnostic path may expose.
    token: String,
    /// Public value used by the redacted snapshot renderer.
    value: u32,
}

impl fmt::Debug for Secret {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("secret Debug must never run")
    }
}

/// Changes the visible value without reading or formatting the secret.
struct SecretFeature;

impl Reducer for SecretFeature {
    type State = Secret;
    type Action = bool;
    fn reduce(
        &mut self,
        state: &mut Secret,
        action: &bool,
        _: &mut ReduceContext<'_>,
    ) -> Effect<bool> {
        if *action {
            Effect::send(false)
        } else {
            state.value += 1;
            Effect::none()
        }
    }
}

#[test]
fn custom_renderer_redacts_entire_payload_without_debug_in_all_assertion_modes() {
    for projected in [false, true] {
        for receive in [false, true] {
            let message = panic_message(
                catch_unwind(AssertUnwindSafe(|| {
                    let state = Secret {
                        token: "sentinel-private-value".to_owned(),
                        value: 0,
                    };
                    let builder = if projected {
                        TestStore::projected_builder(state, SecretFeature, Clone::clone)
                    } else {
                        TestStore::builder(state, SecretFeature)
                    };
                    let mut store = builder
                        .with_snapshot_renderer(|s| {
                            format!("value: {}\ntoken: [redacted]", s.value)
                        })
                        .build();
                    if receive {
                        let _task = block_on(store.send(true)).unwrap();
                        let _task = block_on(store.receive(|_| true)).unwrap();
                    } else {
                        let _task = block_on(store.send(false)).unwrap();
                    }
                }))
                .unwrap_err(),
            );
            assert!(message.contains("state mismatch"), "{message}");
            assert!(message.contains("[redacted]"), "{message}");
            assert!(!message.contains("sentinel-private-value"), "{message}");
            assert!(!message.contains("secret Debug"), "{message}");
        }
    }
}

#[test]
fn renderer_panic_retains_pending_assertion_and_refuses_later_updates() {
    for receive in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let rendered = Arc::clone(&calls);
        let mut store = TestStore::builder(String::new(), Text)
            .with_snapshot_renderer(move |_| {
                rendered.fetch_add(1, Ordering::SeqCst);
                panic!("renderer panic")
            })
            .build();
        if receive {
            let _task = block_on(store.send(Action::Start("changed".to_owned()))).unwrap();
        }
        let message = panic_message(
            catch_unwind(AssertUnwindSafe(|| {
                if receive {
                    block_on(store.receive(|_| true))
                } else {
                    block_on(store.send(Action::Set("changed".to_owned())))
                }
            }))
            .unwrap_err(),
        );
        assert_eq!(message, "renderer panic");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let error = block_on(store.finish()).unwrap_err();
        assert!(error.is_incomplete_assertion());
        let error =
            block_on(store.send_with(Action::Set(String::new()), |_| panic!("later update ran")))
                .unwrap_err();
        assert!(error.is_incomplete_assertion());
        let teardown = panic_message(
            catch_unwind(AssertUnwindSafe(|| drop(store)))
                .expect_err("Drop must retain the assertion interrupted by the renderer"),
        );
        let operation = if receive { "receive" } else { "send" };
        assert!(teardown.starts_with(&format!(
            "TestStore teardown failed: TestStore incomplete assertion for {operation} at "
        )));
    }
}

#[test]
fn mismatch_payload_preserves_source_operation_coverage_and_runtime() {
    let mut payloads = Vec::new();
    for projected in [false, true] {
        for receive in [false, true] {
            let mut store = if projected {
                TestStore::projected(String::new(), Text, Clone::clone)
            } else {
                TestStore::new(String::new(), Text)
            };
            if receive {
                let _task = block_on(store.send(Action::Start("changed".to_owned()))).unwrap();
            }
            let origin_line = line!() + 2;
            let future: std::pin::Pin<Box<dyn Future<Output = _> + '_>> = if receive {
                Box::pin(store.receive(|_| true))
            } else {
                Box::pin(store.send(Action::Set("changed".to_owned())))
            };
            let message =
                panic_message(catch_unwind(AssertUnwindSafe(|| block_on(future))).unwrap_err());
            assert!(
                message.contains(&format!(
                    "{}:{}:",
                    file!(),
                    origin_line + if receive { 0 } else { 2 }
                )),
                "{message}"
            );
            assert!(
                message.contains(if receive {
                    "during receive"
                } else {
                    "during send"
                }),
                "{message}"
            );
            assert!(
                message.contains(if projected {
                    "projected coverage"
                } else {
                    "full-state coverage"
                }),
                "{message}"
            );
            assert!(message.contains("runtime: Some("), "{message}");
            assert!(message.contains("commit_version:"), "{message}");
            // Only the caller's file/line/column changes as this test is edited.
            let context = message.split_once(" during ").unwrap().1;
            payloads.push(format!(
                "TestStore state mismatch at <source> during {context}"
            ));
            let teardown = panic_message(
                catch_unwind(AssertUnwindSafe(|| drop(store)))
                    .expect_err("Drop must retain the mismatched assertion"),
            );
            let operation = if receive { "receive" } else { "send" };
            assert!(teardown.starts_with(&format!(
                "TestStore teardown failed: TestStore incomplete assertion for {operation} at "
            )));
        }
    }
    insta::assert_snapshot!(payloads.join("\n\n"), @r#"
    TestStore state mismatch at <source> during send: full-state coverage
    expected:
    - 1 | ""
    actual:
    + 1 | "changed"
    Later differences may be omitted.
    runtime: Some(ActorDiagnostics { runtime: RuntimeSnapshot { commit_version: 1, live_task_ids: [], cancellation_count: 0, timer_deadlines: [], observation_count: 0 }, cancellation_identities: [], handle_registration_count: 0 })

    TestStore state mismatch at <source> during receive: full-state coverage
    expected:
    - 1 | ""
    actual:
    + 1 | "changed"
    Later differences may be omitted.
    runtime: Some(ActorDiagnostics { runtime: RuntimeSnapshot { commit_version: 2, live_task_ids: [], cancellation_count: 0, timer_deadlines: [], observation_count: 0 }, cancellation_identities: [], handle_registration_count: 0 })

    TestStore state mismatch at <source> during send: projected coverage (exhaustive over the selected snapshot only)
    expected:
    - 1 | ""
    actual:
    + 1 | "changed"
    Later differences may be omitted.
    runtime: Some(ActorDiagnostics { runtime: RuntimeSnapshot { commit_version: 1, live_task_ids: [], cancellation_count: 0, timer_deadlines: [], observation_count: 0 }, cancellation_identities: [], handle_registration_count: 0 })

    TestStore state mismatch at <source> during receive: projected coverage (exhaustive over the selected snapshot only)
    expected:
    - 1 | ""
    actual:
    + 1 | "changed"
    Later differences may be omitted.
    runtime: Some(ActorDiagnostics { runtime: RuntimeSnapshot { commit_version: 2, live_task_ids: [], cancellation_count: 0, timer_deadlines: [], observation_count: 0 }, cancellation_identities: [], handle_registration_count: 0 })
    "#);
}
