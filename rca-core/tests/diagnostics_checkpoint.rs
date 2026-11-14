#![cfg(feature = "diagnostics")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use rca_core::diagnostics::{CheckpointOutcome, checkpoint, checkpoint_disabled};

#[test]
fn observes_before_operation_after_and_preserves_typed_results() {
    let events = RefCell::new(Vec::new());
    let mut state = 3;
    let result = checkpoint(
        &mut state,
        "save",
        |state| {
            events.borrow_mut().push("capture");
            *state
        },
        |state| {
            events.borrow_mut().push("operation");
            *state += 2;
            Err::<(), _>(Rc::new(17))
        },
        |change| {
            events.borrow_mut().push("sink");
            assert_eq!(change.label(), "save");
            assert_eq!((*change.before(), *change.after()), (3, 5));
            assert!(change.is_changed());
            assert_eq!(change.outcome(), CheckpointOutcome::Error);
            let rendered = format!("{change:?}");
            assert!(rendered.contains("is_changed: true"));
            assert!(rendered.contains("outcome: Error"));
            assert!(!rendered.contains("save"));
        },
    );
    assert_eq!(*result.unwrap_err(), 17);
    assert_eq!(state, 5);
    assert_eq!(
        *events.borrow(),
        if cfg!(debug_assertions) {
            vec!["capture", "operation", "capture", "sink"]
        } else {
            vec!["operation"]
        }
    );
}

#[test]
fn owned_callbacks_drop_once_after_success_or_error_in_both_profiles() {
    struct Probe(Rc<Cell<usize>>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    for succeeds in [false, true] {
        let drops = Rc::new(Cell::new(0));
        let calls = Cell::new(0);
        let capture_calls = &calls;
        let sink_calls = &calls;
        let capture_probe = Probe(Rc::clone(&drops));
        let sink_probe = Probe(Rc::clone(&drops));
        let payload = Rc::new(17);
        let mut state = 0;
        let result = checkpoint(
            &mut state,
            "owned callbacks",
            move |state| {
                std::hint::black_box(&capture_probe);
                capture_calls.set(capture_calls.get() + 1);
                *state
            },
            |state| {
                *state += 1;
                assert_eq!(drops.get(), 0);
                if succeeds {
                    Ok(Rc::clone(&payload))
                } else {
                    Err(Rc::clone(&payload))
                }
            },
            move |_| {
                std::hint::black_box(&sink_probe);
                sink_calls.set(sink_calls.get() + 1);
            },
        );

        assert_eq!(result.is_ok(), succeeds);
        let returned = match result {
            Ok(value) | Err(value) => value,
        };
        assert!(Rc::ptr_eq(&returned, &payload));
        assert_eq!(state, 1);
        assert_eq!(drops.get(), 2);
        assert_eq!(calls.get(), if cfg!(debug_assertions) { 3 } else { 0 });
    }
}

#[test]
fn destructor_panics_release_the_unreturned_result_without_reordering_other_cleanup() {
    struct Owned {
        label: &'static str,
        panic_at: &'static str,
        drops: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.label);
            assert_ne!(self.label, self.panic_at, "owned destructor");
        }
    }

    struct Snapshot {
        value: usize,
        _owner: Owned,
    }

    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    for succeeds in [false, true] {
        for panic_at in ["none", "before", "after", "capture", "sink"] {
            let drops = Rc::new(RefCell::new(Vec::new()));
            let owned = |label| Owned {
                label,
                panic_at,
                drops: Rc::clone(&drops),
            };
            let capture_probe = owned("capture");
            let sink_probe = owned("sink");
            let make_snapshot_owner = &owned;
            let mut state = 0;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                checkpoint(
                    &mut state,
                    "destructor",
                    move |state| {
                        std::hint::black_box(&capture_probe);
                        Snapshot {
                            value: *state,
                            _owner: make_snapshot_owner(if *state == 0 {
                                "before"
                            } else {
                                "after"
                            }),
                        }
                    },
                    |state| {
                        *state += 1;
                        if succeeds {
                            Ok(owned("result"))
                        } else {
                            Err(owned("result"))
                        }
                    },
                    move |_| {
                        std::hint::black_box(&sink_probe);
                    },
                )
            }));
            let should_panic = panic_at != "none"
                && (cfg!(debug_assertions) || matches!(panic_at, "capture" | "sink"));
            assert_eq!(outcome.is_err(), should_panic);
            if !should_panic {
                let result = outcome.unwrap();
                assert_eq!(result.is_ok(), succeeds);
                drop(result);
            }

            assert_eq!(state, 1);
            let drops = drops.borrow();
            assert_eq!(
                drops.iter().filter(|label| **label == "result").count(),
                1,
                "result must be released after {panic_at} panic (success={succeeds})"
            );
            let other_cleanup: Vec<_> = drops
                .iter()
                .copied()
                .filter(|label| *label != "result")
                .collect();
            assert_eq!(
                other_cleanup,
                if cfg!(debug_assertions) {
                    vec!["after", "before", "sink", "capture"]
                } else {
                    vec!["capture", "sink"]
                },
                "cleanup order for {panic_at} panic"
            );
        }
    }
}

#[test]
fn disabled_runs_once_with_unbounded_local_state_and_keeps_error_identity() {
    struct Local<'a>(&'a Cell<usize>, Rc<()>);
    let count = Cell::new(0);
    let marker = Rc::new(());
    let mut state = Local(&count, Rc::clone(&marker));
    let error = Rc::new("failure");
    let result = checkpoint_disabled(&mut state, |state| {
        state.0.set(state.0.get() + 1);
        assert!(Rc::ptr_eq(&state.1, &marker));
        Err::<(), _>(Rc::clone(&error))
    });
    assert_eq!(count.get(), 1);
    assert!(Rc::ptr_eq(&result.unwrap_err(), &error));
}

#[test]
fn nested_explicit_labels_deliver_inner_before_outer() {
    let labels = RefCell::new(Vec::new());
    let mut state = 0;
    let result = checkpoint(
        &mut state,
        "outer",
        |s| *s,
        |s| {
            checkpoint(
                s,
                "outer.inner",
                |s| *s,
                |s| {
                    *s += 1;
                    Ok::<_, ()>(23)
                },
                |event| {
                    assert_eq!(event.outcome(), CheckpointOutcome::Success);
                    labels.borrow_mut().push(event.label().to_owned());
                },
            )
        },
        |event| labels.borrow_mut().push(event.label().to_owned()),
    );
    assert_eq!(result, Ok(23));
    assert_eq!(
        *labels.borrow(),
        if cfg!(debug_assertions) {
            vec!["outer.inner", "outer"]
        } else {
            vec![]
        }
    );
}

#[test]
fn required_operation_assertion_executes_in_both_profiles_and_disabled() {
    for disabled in [false, true] {
        let mut state = 0;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if disabled {
                checkpoint_disabled(&mut state, |state| -> Result<(), ()> {
                    *state += 1;
                    panic!("required assertion");
                })
            } else {
                checkpoint(
                    &mut state,
                    "assert",
                    |s| *s,
                    |state| -> Result<(), ()> {
                        *state += 1;
                        panic!("required assertion");
                    },
                    |_| panic!("sink must not run"),
                )
            }
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"required assertion"));
        assert_eq!(state, 1);
    }
}

#[test]
fn unchanged_snapshot_still_delivers_without_debug_domain_bounds() {
    struct Snapshot(Cell<usize>);
    impl PartialEq for Snapshot {
        fn eq(&self, _: &Self) -> bool {
            self.0.set(self.0.get() + 1);
            true
        }
    }
    let calls = Cell::new(0);
    let mut state = Rc::new(());
    let result = checkpoint(
        &mut state,
        "secret label",
        |_| Snapshot(Cell::new(0)),
        |_| Ok::<_, ()>(()),
        |event| {
            calls.set(calls.get() + 1);
            assert!(!event.is_changed());
            assert_eq!(event.before().0.get(), 1);
            let rendered = format!("{event:?}");
            assert!(rendered.contains("Success"));
            assert!(!rendered.contains("secret label"));
            assert_eq!(event.before().0.get(), 1);
        },
    );
    assert_eq!(result, Ok(()));
    assert_eq!(calls.get(), usize::from(cfg!(debug_assertions)));
}

#[test]
fn panic_stages_preserve_mutations_and_stop_subsequent_callbacks() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    struct Snapshot {
        value: usize,
        comparison_panics: bool,
    }
    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            assert!(!self.comparison_panics, "comparison");
            self.value == other.value
        }
    }
    for stage in ["before", "operation", "after", "comparison", "sink"] {
        let captures = Cell::new(0);
        let operations = Cell::new(0);
        let sinks = Cell::new(0);
        let mut state = 0;
        let result = catch_unwind(AssertUnwindSafe(|| {
            checkpoint(
                &mut state,
                "panic",
                |state| {
                    captures.set(captures.get() + 1);
                    assert!(
                        stage != "before" && !(stage == "after" && captures.get() == 2),
                        "capture"
                    );
                    Snapshot {
                        value: *state,
                        comparison_panics: stage == "comparison",
                    }
                },
                |state| {
                    operations.set(operations.get() + 1);
                    *state += 1;
                    assert_ne!(stage, "operation", "operation");
                    Ok::<_, ()>(42)
                },
                |_| {
                    sinks.set(sinks.get() + 1);
                    panic!("sink");
                },
            )
        }));
        if cfg!(debug_assertions) {
            assert!(result.is_err());
            assert_eq!(operations.get(), usize::from(stage != "before"));
            assert_eq!(state, usize::from(stage != "before"));
            assert_eq!(
                captures.get(),
                if stage == "before" || stage == "operation" {
                    1
                } else {
                    2
                }
            );
            assert_eq!(sinks.get(), usize::from(stage == "sink"));
        } else {
            assert_eq!(result.is_err(), stage == "operation");
            assert_eq!(operations.get(), 1);
            assert_eq!(state, 1);
            assert_eq!((captures.get(), sinks.get()), (0, 0));
        }
    }
}
