//! Ordered stack operations, route codecs and dismissal scoped to the nearest owner.

#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{
    ActionPath, Dismiss, Effect, EncodedStackRoute, ReduceContext, Reducer, StackAction,
    StackEncoding, StackId, StackState, StatePath, Store, stack,
};
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use thread_runtime::ThreadRuntime;

/// Move-only route state; equal visible values still need distinct stack IDs.
struct Route {
    name: &'static str,
    count: u8,
}

/// Move-only push input, separate from the route created by the adapter.
struct Push(&'static str);

/// Child work and captured dismissal clients, without `Clone` or `Debug` bounds.
enum ChildAction {
    Increment,
    Capture(Arc<Mutex<Option<Dismiss>>>),
    Replay(Dismiss),
}

/// Wraps stack actions at the parent boundary.
enum Action {
    Path(StackAction<Push, ChildAction>),
}

/// Ordered routes, an execution trace and parent-side replacement controls.
struct State {
    path: StackState<Route>,
    trace: Arc<Mutex<Vec<String>>>,
    replace_on_pop: bool,
    insert_unknown_on_pop: bool,
}

/// Records parent entry and can replace the route targeted by a pop.
struct Parent;
impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        let Action::Path(action) = action;
        let operation = match action {
            StackAction::Push { .. } => "push",
            StackAction::Element { .. } => "element",
            StackAction::PopFrom { .. } => "from",
            StackAction::PopTo { .. } => "to",
        };
        let values: Vec<_> = state
            .path
            .values()
            .map(|route| format!("{}:{}", route.name, route.count))
            .collect();
        state
            .trace
            .lock()
            .unwrap()
            .push(format!("base:{operation}:{}", values.join(",")));
        if let StackAction::PopFrom { id } = action {
            if state.replace_on_pop {
                state.path.try_pop_from(id).unwrap();
                state
                    .path
                    .try_push(
                        id.clone(),
                        Route {
                            name: "new",
                            count: 9,
                        },
                    )
                    .unwrap_or_else(|_| panic!("replacement ID was removed"));
            } else if state.insert_unknown_on_pop {
                state
                    .path
                    .try_push(
                        id.clone(),
                        Route {
                            name: "new",
                            count: 9,
                        },
                    )
                    .unwrap_or_else(|_| panic!("unknown target remains available for insertion"));
            }
        }
        Effect::none()
    }
}

/// Records child entry or captures a dismissal client for later replay.
struct Child(Arc<Mutex<Vec<String>>>);
impl Reducer for Child {
    type State = Route;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Route,
        action: &ChildAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Capture(captured) => {
                *captured.lock().unwrap() = Some(context.dismiss());
                return Effect::none();
            }
            ChildAction::Replay(dismiss) => return dismiss.effect(),
            ChildAction::Increment => {}
        }
        self.0
            .lock()
            .unwrap()
            .push(format!("child:{}:{}", state.name, state.count));
        state.count += 1;
        Effect::none()
    }
}

/// Builds the adapter with a separate trace entry for route initialisation.
fn reducer(
    trace: Arc<Mutex<Vec<String>>>,
) -> impl Reducer<State = State, Action = Action> + Send + std::fmt::Debug {
    let initialise_trace = Arc::clone(&trace);
    stack(
        Parent,
        StatePath::new(
            |state: &State| &state.path,
            |state: &mut State| &mut state.path,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Path(action) => Some(action),
            },
            Action::Path,
        ),
        move |input: &Push, _: &mut ReduceContext<'_>| {
            initialise_trace
                .lock()
                .unwrap()
                .push(format!("initialise:{}", input.0));
            Route {
                name: input.0,
                count: 0,
            }
        },
        Child(trace),
    )
}

/// Creates an empty path with parent-side replacement disabled.
fn state(trace: &Arc<Mutex<Vec<String>>>) -> State {
    State {
        path: StackState::new(),
        trace: Arc::clone(trace),
        replace_on_pop: false,
        insert_unknown_on_pop: false,
    }
}

#[test]
fn equal_route_values_have_distinct_ids_and_checked_suffix_operations() {
    let mut path = StackState::new();
    let first = path.push(7);
    let second = path.push(7);
    let third = path.push(9);
    assert_ne!(first, second);
    assert_eq!(path.values().copied().collect::<Vec<_>>(), [7, 7, 9]);
    assert!(path.try_pop_count(4).unwrap_err().is_over_pop());
    assert_eq!(path.values().copied().collect::<Vec<_>>(), [7, 7, 9]);
    assert!(path.try_pop_count(0).unwrap().is_empty());
    assert_eq!(path.try_pop_to(&second).unwrap(), [(third, 9)]);
    assert_eq!(path.try_pop_from(&second).unwrap(), [(second.clone(), 7)]);
    assert_eq!(path.pop(), Some((first, 7)));
    assert_eq!(path.pop(), None);
    assert!(path.try_pop_from(&second).unwrap_err().is_unknown_id());
    assert!(path.try_pop_to(&second).unwrap_err().is_unknown_id());
}

#[test]
fn default_stack_ids_and_state_preserve_ordered_move_only_values() {
    /// Keeps an allocation identity that can be checked before and after a move.
    struct Payload(Box<u8>);
    let mut path = StackState::<Payload>::default();
    assert_eq!(path.len(), 0);
    assert!(path.is_empty());
    let first = StackId::default();
    let second = StackId::default();
    assert_ne!(first, second);
    assert_eq!(first.clone(), first);
    let first_value = Payload(Box::new(7));
    let second_value = Payload(Box::new(7));
    let first_allocation = std::ptr::from_ref(first_value.0.as_ref());
    let second_allocation = std::ptr::from_ref(second_value.0.as_ref());
    assert_ne!(first_allocation, second_allocation);
    path.try_push(first.clone(), first_value)
        .unwrap_or_else(|_| panic!("fresh first ID"));
    path.try_push(second.clone(), second_value)
        .unwrap_or_else(|_| panic!("fresh second ID"));
    assert!(!path.is_empty());
    assert_eq!(path.len(), 2);
    assert_eq!(path.ids().collect::<Vec<_>>(), [&first, &second]);
    assert_eq!(
        std::ptr::from_ref(path.get(&first).unwrap().0.as_ref()),
        first_allocation
    );
    assert_eq!(
        std::ptr::from_ref(path.get(&second).unwrap().0.as_ref()),
        second_allocation
    );
    assert_eq!(
        path.values().map(|value| *value.0).collect::<Vec<_>>(),
        [7, 7]
    );
    let (id, second_value) = path.pop().unwrap();
    assert_eq!(id, second);
    assert_eq!(
        std::ptr::from_ref(second_value.0.as_ref()),
        second_allocation
    );
    assert!(!path.is_empty());
    assert_eq!(path.len(), 1);
    assert_eq!(path.ids().collect::<Vec<_>>(), [&first]);
    assert_eq!(
        std::ptr::from_ref(path.get(&first).unwrap().0.as_ref()),
        first_allocation
    );
    let (id, first_value) = path.pop().unwrap();
    assert_eq!(id, first);
    assert_eq!(std::ptr::from_ref(first_value.0.as_ref()), first_allocation);
    assert_eq!(path.len(), 0);
    assert!(path.is_empty());
    assert!(path.pop().is_none());
    drop((first_value, second_value));
}

#[test]
fn stack_diagnostics_preserve_private_move_only_routes() {
    let mut path = StackState::new();
    let route = Box::new(Route {
        name: "private-stack-route",
        count: 7,
    });
    let allocation = std::ptr::from_ref(route.as_ref());
    let id = path.push(route);
    assert_eq!(format!("{id:?}"), "StackId(..)");
    let rendered = format!("{path:?}");
    assert!(rendered.contains("len: 1"));
    assert!(!rendered.contains("private-stack-route"));

    let over_pop = path.try_pop_count(2).err().expect("count exceeds length");
    let unknown = path
        .try_pop_from(&StackId::new())
        .err()
        .expect("ID is absent");
    assert!(over_pop.is_over_pop());
    assert!(!over_pop.is_unknown_id());
    assert!(unknown.is_unknown_id());
    assert!(!unknown.is_over_pop());
    let over_pop_message = over_pop.to_string();
    let unknown_message = unknown.to_string();
    assert_ne!(
        over_pop_message.lines().next(),
        unknown_message.lines().next()
    );
    for error in [over_pop, unknown] {
        assert!(error.source().is_none());
        assert!(!error.backtrace().to_string().is_empty());
        assert!(!error.to_string().contains("private-stack-route"));
    }
    assert_eq!(path.ids().collect::<Vec<_>>(), [&id]);
    let (retained_id, retained) = path.pop().unwrap();
    assert_eq!(retained_id, id);
    assert_eq!(std::ptr::from_ref(retained.as_ref()), allocation);
    assert_eq!(retained.name, "private-stack-route");
    assert_eq!(retained.count, 7);
    assert!(path.is_empty());
}

#[test]
fn duplicate_owned_push_recovers_inputs_without_replacing_existing_route() {
    let mut path = StackState::new();
    let id = path.push(Route {
        name: "original",
        count: 1,
    });
    let (rejected_id, rejected) = path
        .try_push(
            id.clone(),
            Route {
                name: "rejected",
                count: 2,
            },
        )
        .unwrap_err()
        .into_parts();
    assert_eq!(rejected_id, id);
    assert_eq!(rejected.name, "rejected");
    assert_eq!(path.get(&id).unwrap().name, "original");
}

#[test]
fn push_child_and_pop_reduction_have_literal_ordered_traces() {
    let runtime = ThreadRuntime::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let reducer = reducer(Arc::clone(&trace));
    assert_eq!(format!("{reducer:?}"), "Stack { .. }");
    assert!(trace.lock().unwrap().is_empty());
    let store = Store::new(state(&trace), reducer, runtime.runtime());
    let a = StackId::new();
    let b = StackId::new();
    let c = StackId::new();
    futures_executor::block_on(async {
        for (id, name) in [(a.clone(), "A"), (b.clone(), "B"), (c.clone(), "C")] {
            store
                .send(Action::Path(StackAction::Push {
                    id,
                    input: Push(name),
                }))
                .await
                .unwrap()
                .finish()
                .await;
        }
        store
            .send(Action::Path(StackAction::Element {
                id: b.clone(),
                action: ChildAction::Increment,
            }))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(Action::Path(StackAction::PopTo { id: b.clone() }))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(Action::Path(StackAction::PopFrom { id: b }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            trace.lock().unwrap().as_slice(),
            [
                "initialise:A",
                "base:push:A:0",
                "initialise:B",
                "base:push:A:0,B:0",
                "initialise:C",
                "base:push:A:0,B:0,C:0",
                "child:B:0",
                "base:element:A:0,B:1,C:0",
                "base:to:A:0,B:1,C:0",
                "base:from:A:0,B:1",
            ]
        );
        assert_eq!(
            store
                .read(|state| state.path.values().map(|v| v.name).collect::<Vec<_>>())
                .await
                .unwrap(),
            ["A"]
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn duplicate_push_and_unknown_element_report_before_base_without_child_work() {
    let runtime = ThreadRuntime::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut initial = state(&trace);
    let a = initial.path.push(Route {
        name: "A",
        count: 1,
    });
    let issue_trace = Arc::clone(&trace);
    let store = Store::builder(initial, reducer(Arc::clone(&trace)), runtime.runtime())
        .with_issue_handler(move |issue| issue_trace.lock().unwrap().push(issue.code.to_owned()))
        .build()
        .unwrap();
    futures_executor::block_on(async {
        store
            .send(Action::Path(StackAction::Push {
                id: a,
                input: Push("bad"),
            }))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(Action::Path(StackAction::Element {
                id: StackId::new(),
                action: ChildAction::Increment,
            }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            trace.lock().unwrap().as_slice(),
            [
                "rca.stack_duplicate_id",
                "base:push:A:1",
                "rca.missing_child",
                "base:element:A:1",
            ]
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn pop_does_not_remove_a_same_id_replacement_installed_by_parent() {
    let runtime = ThreadRuntime::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut initial = state(&trace);
    initial.path.push(Route {
        name: "A",
        count: 1,
    });
    let b = initial.path.push(Route {
        name: "B",
        count: 2,
    });
    initial.replace_on_pop = true;
    let store = Store::new(initial, reducer(Arc::clone(&trace)), runtime.runtime());
    futures_executor::block_on(async {
        store
            .send(Action::Path(StackAction::PopFrom { id: b }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state.path.values().map(|v| v.name).collect::<Vec<_>>())
                .await
                .unwrap(),
            ["A", "new"]
        );
        assert_eq!(trace.lock().unwrap().as_slice(), ["base:from:A:1,B:2"]);
        store.shutdown().await.unwrap();
    });
}

#[test]
fn unknown_pop_reports_then_preserves_the_target_inserted_by_parent() {
    let runtime = ThreadRuntime::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let mut initial = state(&trace);
    initial.insert_unknown_on_pop = true;
    let issue_trace = Arc::clone(&trace);
    let store = Store::builder(initial, reducer(Arc::clone(&trace)), runtime.runtime())
        .with_issue_handler(move |issue| issue_trace.lock().unwrap().push(issue.code.to_owned()))
        .build()
        .unwrap();
    futures_executor::block_on(async {
        store
            .send(Action::Path(StackAction::PopFrom { id: StackId::new() }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state.path.values().map(|v| v.name).collect::<Vec<_>>())
                .await
                .unwrap(),
            ["new"]
        );
        assert_eq!(
            trace.lock().unwrap().as_slice(),
            ["rca.stack_unknown_id", "base:from:"]
        );
        store.shutdown().await.unwrap();
    });
}

/// Small caller-owned route format used to test the stack envelope separately.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum SavedRoute {
    Detail(u8),
    Settings,
}

/// Distinguishes errors from the caller's route decoder and raw input parser.
#[derive(Debug, Eq, PartialEq)]
enum CodecError {
    Tag,
    Version,
    Payload,
    OddInput,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CodecError {}

/// Encodes an explicit version and tag rather than a process-local route ID.
fn encode_route(route: &SavedRoute) -> EncodedStackRoute {
    let (tag, payload) = match route {
        SavedRoute::Detail(value) => ("detail", vec![*value]),
        SavedRoute::Settings => ("settings", Vec::new()),
    };
    EncodedStackRoute {
        tag: tag.to_owned(),
        version: 1,
        payload,
    }
}

/// Rejects unknown versions, tags and payload shapes at the route boundary.
fn decode_route(route: &EncodedStackRoute) -> Result<SavedRoute, CodecError> {
    if route.version != 1 {
        return Err(CodecError::Version);
    }
    match (route.tag.as_str(), route.payload.as_slice()) {
        ("detail", [value]) => Ok(SavedRoute::Detail(*value)),
        ("settings", []) => Ok(SavedRoute::Settings),
        ("detail" | "settings", _) => Err(CodecError::Payload),
        _ => Err(CodecError::Tag),
    }
}

#[test]
fn codec_preserves_ordered_duplicate_values_but_regenerates_all_logical_ids() {
    let mut original = StackState::new();
    original.push(SavedRoute::Detail(7));
    original.push(SavedRoute::Detail(7));
    original.push(SavedRoute::Settings);
    let encoded = original
        .encode_with(|route| Ok::<_, CodecError>(encode_route(route)))
        .unwrap();
    assert_eq!(
        encoded,
        StackEncoding {
            version: 1,
            routes: vec![
                EncodedStackRoute {
                    tag: "detail".into(),
                    version: 1,
                    payload: vec![7]
                },
                EncodedStackRoute {
                    tag: "detail".into(),
                    version: 1,
                    payload: vec![7]
                },
                EncodedStackRoute {
                    tag: "settings".into(),
                    version: 1,
                    payload: vec![]
                },
            ],
        }
    );
    let decoded = StackState::decode_with(encoded, |route| decode_route(&route)).unwrap();
    assert_eq!(
        decoded.values().cloned().collect::<Vec<_>>(),
        [
            SavedRoute::Detail(7),
            SavedRoute::Detail(7),
            SavedRoute::Settings,
        ]
    );
    assert!(decoded.ids().all(|id| !original.ids().any(|old| old == id)));
    assert_ne!(original, decoded);
    let cloned = original.clone();
    assert_eq!(original, cloned);
    let mut original_hash = std::collections::hash_map::DefaultHasher::new();
    let mut cloned_hash = std::collections::hash_map::DefaultHasher::new();
    original.hash(&mut original_hash);
    cloned.hash(&mut cloned_hash);
    assert_eq!(original_hash.finish(), cloned_hash.finish());
}

#[test]
fn codec_rejects_unknown_envelope_before_invoking_the_route_decoder() {
    let mut calls = 0;
    let decoded: Result<StackState<SavedRoute>, _> = StackState::decode_with(
        StackEncoding {
            version: 2,
            routes: vec![EncodedStackRoute {
                tag: "detail".into(),
                version: 1,
                payload: vec![7],
            }],
        },
        |route| {
            calls += 1;
            decode_route(&route)
        },
    );
    let error = decoded.unwrap_err();
    assert!(error.is_unsupported_version());
    assert_eq!(error.route_index(), None);
    assert!(error.codec_error().is_none());
    assert!(error.source().is_none());
    let debug = format!("{error:?}");
    assert!(debug.contains("StackDecodeError"));
    assert!(debug.contains("route_index: None"));
    assert!(!debug.contains("Tag"));
    let display = error.to_string();
    assert!(display.starts_with("unsupported stack envelope version"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(error.into_codec_error().is_none());
    assert_eq!(calls, 0);
}

#[test]
fn closed_route_codec_errors_retain_failing_index_and_original_typed_cause() {
    for (tag, version, payload, expected) in [
        ("unknown", 1, vec![], CodecError::Tag),
        ("detail", 2, vec![7], CodecError::Version),
        ("detail", 1, vec![], CodecError::Payload),
        ("detail", 1, vec![7, 8], CodecError::Payload),
        ("settings", 1, vec![7], CodecError::Payload),
    ] {
        let input = StackEncoding {
            version: 1,
            routes: vec![
                EncodedStackRoute {
                    tag: "settings".into(),
                    version: 1,
                    payload: vec![],
                },
                EncodedStackRoute {
                    tag: tag.into(),
                    version,
                    payload,
                },
            ],
        };
        let mut calls = 0;
        let error = StackState::decode_with(input, |route| {
            calls += 1;
            decode_route(&route)
        })
        .unwrap_err();
        assert!(!error.is_unsupported_version());
        assert_eq!(error.route_index(), Some(1));
        assert_eq!(error.codec_error(), Some(&expected));
        let cause = error.codec_error().unwrap();
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<CodecError>()
            .unwrap();
        assert!(std::ptr::eq(cause, source));
        let debug = format!("{error:?}");
        assert!(debug.contains("StackDecodeError"));
        assert!(debug.contains("route_index: Some(1)"));
        let display = error.to_string();
        assert!(display.starts_with("stack route decoding failed at index 1"));
        assert!(display.ends_with(&error.backtrace().to_string()));
        for marker in ["Tag", "Version", "Payload"] {
            assert!(!debug.contains(marker));
            assert!(!display.contains(marker));
        }
        assert_eq!(error.into_codec_error(), Some(expected));
        assert_eq!(calls, 2);
    }
}

#[test]
fn codec_failure_releases_decoded_prefix_and_preserves_move_only_error() {
    use std::cell::{Cell, RefCell};

    /// Records disposal of the successfully decoded prefix.
    struct Value<'a>(&'a RefCell<Vec<&'static str>>);
    impl Drop for Value<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push("value dropped");
        }
    }

    /// Preserves a move-only error payload until its caller disposes it.
    struct Failure<'a> {
        payload: Box<u8>,
        events: &'a RefCell<Vec<&'static str>>,
    }
    impl Drop for Failure<'_> {
        fn drop(&mut self) {
            self.events.borrow_mut().push("error dropped");
        }
    }

    let events = RefCell::new(Vec::new());
    let original_payload = Cell::new(std::ptr::null());
    let input = StackEncoding {
        version: 1,
        routes: ["valid", "rejected", "unvisited"]
            .map(|tag| EncodedStackRoute {
                tag: tag.to_owned(),
                version: 1,
                payload: Vec::new(),
            })
            .into(),
    };
    let result = StackState::decode_with(input, |route| match route.tag.as_str() {
        "valid" => {
            events.borrow_mut().push("valid decoded");
            Ok(Value(&events))
        }
        "rejected" => {
            events.borrow_mut().push("route rejected");
            let payload = Box::new(7);
            original_payload.set(std::ptr::from_ref(payload.as_ref()));
            Err(Failure {
                payload,
                events: &events,
            })
        }
        _ => panic!("decoding must stop at the first rejected route"),
    });
    let Err(error) = result else {
        panic!("the caller rejected the second route");
    };

    assert_eq!(error.route_index(), Some(1));
    assert_eq!(
        *events.borrow(),
        ["valid decoded", "route rejected", "value dropped"]
    );
    let borrowed = error.codec_error().unwrap();
    assert_eq!(
        std::ptr::from_ref(borrowed.payload.as_ref()),
        original_payload.get()
    );
    let owned = error.into_codec_error().unwrap();
    assert_eq!(
        std::ptr::from_ref(owned.payload.as_ref()),
        original_payload.get()
    );
    assert_eq!(events.borrow().len(), 3);
    drop(owned);
    assert_eq!(
        *events.borrow(),
        [
            "valid decoded",
            "route rejected",
            "value dropped",
            "error dropped"
        ]
    );

    // Evidence only: these layouts are not stable API or platform-size assertions.
    eprintln!(
        "stack decode layout: borrowed_error={} word_error={} string_error={} word_result={}",
        std::mem::size_of::<rca_core::StackDecodeError<Failure<'_>>>(),
        std::mem::size_of::<rca_core::StackDecodeError<u64>>(),
        std::mem::size_of::<rca_core::StackDecodeError<String>>(),
        std::mem::size_of::<Result<StackState<u8>, rca_core::StackDecodeError<u64>>>(),
    );
}

// Real caller framing boundary: each raw pair is a tag followed by payload bytes.
// RCA's typed envelope deliberately cannot represent an unpaired raw tag.
/// Parses caller-owned tag/payload pairs before handing an envelope to RCA.
fn decode_raw_pairs(input: &[Vec<u8>]) -> Result<StackEncoding, CodecError> {
    let (pairs, remainder) = input.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(CodecError::OddInput);
    }
    let routes = pairs
        .iter()
        .map(|pair| {
            let tag = std::str::from_utf8(&pair[0]).map_err(|_invalid_tag| CodecError::Tag)?;
            Ok(EncodedStackRoute {
                tag: tag.to_owned(),
                version: 1,
                payload: pair[1].clone(),
            })
        })
        .collect::<Result<_, CodecError>>()?;
    Ok(StackEncoding { version: 1, routes })
}

#[test]
fn caller_raw_codec_rejects_odd_input_before_installing_any_decoded_route() {
    let valid = decode_raw_pairs(&[b"detail".to_vec(), vec![7]]).unwrap();
    let decoded = StackState::decode_with(valid, |route| decode_route(&route)).unwrap();
    assert_eq!(
        decoded.values().cloned().collect::<Vec<_>>(),
        [SavedRoute::Detail(7)]
    );
    assert_eq!(
        decode_raw_pairs(&[b"detail".to_vec(), vec![7], b"settings".to_vec()]),
        Err(CodecError::OddInput)
    );
}

#[test]
fn encoded_failure_returns_the_original_error_without_partial_envelope() {
    let mut state = StackState::new();
    state.push(1);
    state.push(2);
    state.push(3);
    let mut observed = Vec::new();
    let result = state.encode_with(|value| {
        observed.push(*value);
        if *value == 2 {
            return Err(CodecError::Payload);
        }
        Ok(EncodedStackRoute {
            tag: "number".into(),
            version: 1,
            payload: vec![*value],
        })
    });
    assert_eq!(result, Err(CodecError::Payload));
    assert_eq!(observed, [1, 2]);
    assert_eq!(state.values().copied().collect::<Vec<_>>(), [1, 2, 3]);
}

#[test]
fn captured_dismiss_cannot_cross_store_even_when_stores_share_one_stack_adapter() {
    /// Reuses the adapter without merging the two stores' dismissal authority.
    struct Shared<R>(Arc<Mutex<R>>);
    impl<R: Reducer> Reducer for Shared<R> {
        type State = R::State;
        type Action = R::Action;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.lock().unwrap().reduce(state, action, context)
        }
    }
    let runtime = ThreadRuntime::new();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let id = StackId::new();
    let initial = || {
        let mut state = state(&trace);
        state
            .path
            .try_push(
                id.clone(),
                Route {
                    name: "A",
                    count: 1,
                },
            )
            .unwrap_or_else(|_| panic!("fresh stack accepts logical ID"));
        state
    };
    let shared = Arc::new(Mutex::new(reducer(Arc::clone(&trace))));
    let source = Store::new(initial(), Shared(Arc::clone(&shared)), runtime.runtime());
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let destination = Store::builder(initial(), Shared(shared), runtime.runtime())
        .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
        .build()
        .unwrap();
    let captured = Arc::new(Mutex::new(None));
    futures_executor::block_on(async {
        source
            .send(Action::Path(StackAction::Element {
                id: id.clone(),
                action: ChildAction::Capture(Arc::clone(&captured)),
            }))
            .await
            .unwrap()
            .finish()
            .await;
        let dismiss = captured.lock().unwrap().take().unwrap();
        destination
            .send(Action::Path(StackAction::Element {
                id: id.clone(),
                action: ChildAction::Replay(dismiss.clone()),
            }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(destination.read(|state| state.path.len()).await.unwrap(), 1);
        assert_eq!(
            issues.lock().unwrap().as_slice(),
            ["rca.dismiss_unavailable"]
        );
        source
            .send(Action::Path(StackAction::Element {
                id,
                action: ChildAction::Replay(dismiss),
            }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(source.read(|state| state.path.len()).await.unwrap(), 0);
        source.shutdown().await.unwrap();
        destination.shutdown().await.unwrap();
    });
}

#[test]
fn presentation_inside_stack_owns_nearest_dismiss_without_popping_route() {
    use rca_core::{PresentationAction, presentation};

    /// Leaves each ancestor unchanged so only the nearest adapter can dismiss.
    struct Noop<S, A>(std::marker::PhantomData<fn(S, A)>);
    impl<S, A> Reducer for Noop<S, A> {
        type State = S;
        type Action = A;
        fn reduce(&mut self, _: &mut S, _: &A, _: &mut ReduceContext<'_>) -> Effect<A> {
            Effect::none()
        }
    }
    /// Requests dismissal from the nested presentation's reduction context.
    struct Close;
    impl Reducer for Close {
        type State = u8;
        type Action = ();
        fn reduce(&mut self, _: &mut u8, (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
            context.dismiss().effect()
        }
    }
    let child = presentation(
        Noop::<Option<u8>, PresentationAction<()>>(std::marker::PhantomData),
        StatePath::new(|state: &Option<u8>| state, |state: &mut Option<u8>| state),
        ActionPath::new(
            |action: &PresentationAction<()>| Some(action),
            |action| action,
        ),
        |_: &u8| &(),
        Close,
    );
    let reducer = stack(
        Noop::<StackState<Option<u8>>, StackAction<(), PresentationAction<()>>>(
            std::marker::PhantomData,
        ),
        StatePath::new(
            |state: &StackState<Option<u8>>| state,
            |state: &mut StackState<Option<u8>>| state,
        ),
        ActionPath::new(
            |action: &StackAction<(), PresentationAction<()>>| Some(action),
            |action| action,
        ),
        |(): &(), _: &mut ReduceContext<'_>| Some(0),
        child,
    );
    let mut state = StackState::new();
    let a = state.push(Some(1));
    state.push(Some(2));
    let runtime = ThreadRuntime::new();
    let store = Store::new(state, reducer, runtime.runtime());
    futures_executor::block_on(async {
        store
            .send(StackAction::Element {
                id: a,
                action: PresentationAction::Presented(()),
            })
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|path| path.values().copied().collect::<Vec<_>>())
                .await
                .unwrap(),
            [None, Some(2)]
        );
        store.shutdown().await.unwrap();
    });
}

// A second Store allocates its decoy site before visiting the shared adapter.
// Reusing the first Store's numeric node would alias that unrelated live site.
/// Forces stores sharing an adapter to allocate stack slots in different orders.
mod prepopulated_store {
    use super::*;
    /// A target stack plus a decoy that takes the destination's first slot.
    struct Paths {
        target: StackState<u8>,
        decoy: StackState<u8>,
    }
    /// Selects which independent stack receives a child action.
    enum Input {
        Target(StackAction<(), Close>),
        Decoy(StackAction<(), Close>),
    }
    /// Captures or replays the nearest stack's dismissal client.
    enum Close {
        Capture(Arc<Mutex<Option<Dismiss>>>),
        Replay(Dismiss),
    }
    /// Exposes dismissal clients without otherwise changing route state.
    struct Child;
    impl Reducer for Child {
        type State = u8;
        type Action = Close;
        fn reduce(
            &mut self,
            _: &mut u8,
            action: &Close,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Close> {
            match action {
                Close::Capture(captured) => {
                    *captured.lock().unwrap() = Some(context.dismiss());
                    Effect::none()
                }
                Close::Replay(dismiss) => dismiss.effect(),
            }
        }
    }
    /// Leaves both paths unchanged outside their stack adapters.
    struct Parent;
    impl Reducer for Parent {
        type State = Paths;
        type Action = Input;
        fn reduce(&mut self, _: &mut Paths, _: &Input, _: &mut ReduceContext<'_>) -> Effect<Input> {
            Effect::none()
        }
    }
    /// Shares one target adapter between source and destination stores.
    struct Shared<R>(Arc<Mutex<R>>);
    impl<R: Reducer> Reducer for Shared<R> {
        type State = R::State;
        type Action = R::Action;
        fn reduce(
            &mut self,
            state: &mut R::State,
            action: &R::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<R::Action> {
            self.0.lock().unwrap().reduce(state, action, context)
        }
    }
    /// Builds a source target and a destination with decoy then target slots.
    fn stores(
        runtime: &ThreadRuntime,
        target_id: &StackId,
        decoy_id: &StackId,
    ) -> (Store<Paths, Input>, Store<Paths, Input>) {
        let target = stack(
            Parent,
            StatePath::new(
                |state: &Paths| &state.target,
                |state: &mut Paths| &mut state.target,
            ),
            ActionPath::new(
                |action: &Input| match action {
                    Input::Target(action) => Some(action),
                    Input::Decoy(_) => None,
                },
                Input::Target,
            ),
            |(): &(), _: &mut ReduceContext<'_>| 0,
            Child,
        );
        let shared = Arc::new(Mutex::new(target));
        let destination_reducer = stack(
            Shared(Arc::clone(&shared)),
            StatePath::new(
                |state: &Paths| &state.decoy,
                |state: &mut Paths| &mut state.decoy,
            ),
            ActionPath::new(
                |action: &Input| match action {
                    Input::Decoy(action) => Some(action),
                    Input::Target(_) => None,
                },
                Input::Decoy,
            ),
            |(): &(), _: &mut ReduceContext<'_>| 0,
            Child,
        );
        let initial = || {
            let mut target = StackState::new();
            target.try_push(target_id.clone(), 1).unwrap();
            let mut decoy = StackState::new();
            decoy.try_push(decoy_id.clone(), 2).unwrap();
            Paths { target, decoy }
        };
        let source = Store::new(initial(), Shared(shared), runtime.runtime());
        let destination = Store::new(initial(), destination_reducer, runtime.runtime());
        (source, destination)
    }
    #[test]
    fn shared_stack_does_not_alias_a_prepopulated_second_store_slot() {
        let runtime = ThreadRuntime::new();
        let target_id = StackId::new();
        let decoy_id = StackId::new();
        let (source, destination) = stores(&runtime, &target_id, &decoy_id);
        let source_dismiss = Arc::new(Mutex::new(None));
        let target_dismiss = Arc::new(Mutex::new(None));
        let decoy_dismiss = Arc::new(Mutex::new(None));
        futures_executor::block_on(async {
            source
                .send(Input::Target(StackAction::Element {
                    id: target_id.clone(),
                    action: Close::Capture(Arc::clone(&source_dismiss)),
                }))
                .await
                .unwrap()
                .finish()
                .await;
            destination
                .send(Input::Decoy(StackAction::Element {
                    id: decoy_id.clone(),
                    action: Close::Capture(Arc::clone(&decoy_dismiss)),
                }))
                .await
                .unwrap()
                .finish()
                .await;
            destination
                .send(Input::Target(StackAction::Element {
                    id: target_id.clone(),
                    action: Close::Capture(Arc::clone(&target_dismiss)),
                }))
                .await
                .unwrap()
                .finish()
                .await;
            let target = target_dismiss.lock().unwrap().take().unwrap();
            destination
                .send(Input::Target(StackAction::Element {
                    id: target_id.clone(),
                    action: Close::Replay(target),
                }))
                .await
                .unwrap()
                .finish()
                .await;
            assert_eq!(
                destination
                    .read(|paths| (paths.target.len(), paths.decoy.len()))
                    .await
                    .unwrap(),
                (0, 1)
            );
            let decoy = decoy_dismiss.lock().unwrap().take().unwrap();
            destination
                .send(Input::Decoy(StackAction::Element {
                    id: decoy_id,
                    action: Close::Replay(decoy),
                }))
                .await
                .unwrap()
                .finish()
                .await;
            assert_eq!(
                destination
                    .read(|paths| (paths.target.len(), paths.decoy.len()))
                    .await
                    .unwrap(),
                (0, 0)
            );
            let original = source_dismiss.lock().unwrap().take().unwrap();
            source
                .send(Input::Target(StackAction::Element {
                    id: target_id,
                    action: Close::Replay(original),
                }))
                .await
                .unwrap()
                .finish()
                .await;
            assert_eq!(
                source
                    .read(|paths| (paths.target.len(), paths.decoy.len()))
                    .await
                    .unwrap(),
                (0, 1)
            );
            source.shutdown().await.unwrap();
            destination.shutdown().await.unwrap();
        });
    }
}

#[test]
fn stack_inside_presentation_owns_nearest_dismiss_without_clearing_presentation() {
    use rca_core::{PresentationAction, presentation};

    /// Leaves each ancestor unchanged so only the nearest adapter can dismiss.
    struct Noop<S, A>(std::marker::PhantomData<fn(S, A)>);
    impl<S, A> Reducer for Noop<S, A> {
        type State = S;
        type Action = A;
        fn reduce(&mut self, _: &mut S, _: &A, _: &mut ReduceContext<'_>) -> Effect<A> {
            Effect::none()
        }
    }
    /// Requests dismissal from the nested stack's reduction context.
    struct Close;
    impl Reducer for Close {
        type State = u8;
        type Action = ();
        fn reduce(&mut self, _: &mut u8, (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
            context.dismiss().effect()
        }
    }
    let child = stack(
        Noop::<StackState<u8>, StackAction<(), ()>>(std::marker::PhantomData),
        StatePath::new(
            |state: &StackState<u8>| state,
            |state: &mut StackState<u8>| state,
        ),
        ActionPath::new(|action: &StackAction<(), ()>| Some(action), |action| action),
        |(): &(), _: &mut ReduceContext<'_>| 0,
        Close,
    );
    let reducer = presentation(
        Noop::<Option<StackState<u8>>, PresentationAction<StackAction<(), ()>>>(
            std::marker::PhantomData,
        ),
        StatePath::new(
            |state: &Option<StackState<u8>>| state,
            |state: &mut Option<StackState<u8>>| state,
        ),
        ActionPath::new(
            |action: &PresentationAction<StackAction<(), ()>>| Some(action),
            |action| action,
        ),
        |_: &StackState<u8>| &(),
        child,
    );
    let mut path = StackState::new();
    path.push(1);
    let b = path.push(2);
    let runtime = ThreadRuntime::new();
    let store = Store::new(Some(path), reducer, runtime.runtime());
    futures_executor::block_on(async {
        store
            .send(PresentationAction::Presented(StackAction::Element {
                id: b,
                action: (),
            }))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state
                    .as_ref()
                    .map(|path| path.values().copied().collect::<Vec<_>>()))
                .await
                .unwrap(),
            Some(vec![1])
        );
        store.shutdown().await.unwrap();
    });
}

#[test]
fn stack_logical_equality_does_not_include_private_entry_incarnation() {
    let mut original = StackState::new();
    let id = original.push(7_u8);
    let old = original.clone();
    original.pop().unwrap();
    original.try_push(id, 7).unwrap();
    assert_eq!(original, old);
    let mut reordered = StackState::new();
    let a = reordered.push(1_u8);
    let b = reordered.push(2_u8);
    let mut reverse = StackState::new();
    reverse.try_push(b, 2).unwrap();
    reverse.try_push(a, 1).unwrap();
    assert_ne!(reordered, reverse);
}
