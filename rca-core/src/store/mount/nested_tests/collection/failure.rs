//! Faults enter the production collection/DFS, never a replacement interpreter.
use super::*;
use crate::effect::lineage::ControlWake;
use crate::store::command::Command;
use crate::store::ingress::CommandIngress;
use crate::store::lifecycle::CloseState;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;

/// Interrupts a real callback by closing its lifecycle or unwinding once.
#[derive(Clone, Copy, Debug)]
enum Fault {
    Close,
    Panic,
}
/// Arms one callback fault and records the actual reconciliation sequence.
struct Control {
    /// Matches one phase and key, then is consumed before invoking the fault.
    fault: Mutex<Option<(&'static str, u8, Fault)>>,
    lifecycle: Arc<Lifecycle>,
    trace: Trace,
}
impl Control {
    /// Removes the fault under the lock, then closes or panics after releasing it.
    /// Consuming the fault first prevents a second panic during destructor cleanup.
    fn visit(&self, event: &'static str, id: u8) {
        self.trace.lock().push((event, id));
        let fault = {
            let mut fault = self.fault.lock();
            if fault
                .as_ref()
                .is_some_and(|(phase, key, _)| (*phase, *key) == (event, id))
            {
                fault.take()
            } else {
                None
            }
        };
        match fault {
            Some((_, _, Fault::Close)) => {
                self.lifecycle.close(CloseState::Abortive);
            }
            Some((_, _, Fault::Panic)) => panic!("injected {event} {id}"),
            None => {}
        }
    }
}
/// Preserves ID-based equality while exposing real clone, hash and drop boundaries.
struct Key(u8, Arc<Control>);
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Key {}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let lookup_fault_armed = self
            .1
            .fault
            .lock()
            .as_ref()
            .is_some_and(|(phase, _, _)| *phase == "hash");
        if lookup_fault_armed {
            self.1.visit("hash", self.0);
        }
        self.0.hash(state);
    }
}
impl Clone for Key {
    fn clone(&self) -> Self {
        self.1.visit("clone", self.0);
        Self(self.0, std::sync::Arc::clone(&self.1))
    }
}
impl Drop for Key {
    fn drop(&mut self) {
        self.1.visit("key drop", self.0);
    }
}
/// Makes recipe disposal a separately faultable phase of the owned subtree.
struct RecipeCapture(u8, Arc<Control>);
impl Drop for RecipeCapture {
    fn drop(&mut self) {
        self.1.visit("recipe drop", self.0);
    }
}
struct FaultRow {
    child: Option<usize>,
}
/// Records feature disposal separately from configuration, initial and recipe captures.
struct FaultFeature {
    id: u8,
    control: Arc<Control>,
    _capture: Capture,
}
impl Feature for FaultFeature {
    type State = FaultRow;
    type Action = EntryAction;
    fn reduce(
        &mut self,
        _: &mut FaultRow,
        _: &EntryAction,
        _: &mut FeatureContext<'_, FaultRow, EntryAction>,
    ) {
    }
}
impl Drop for FaultFeature {
    fn drop(&mut self) {
        self.control.visit("feature drop", self.id);
    }
}
/// Allows a descendant destructor to interrupt retirement before parent disposal.
struct FaultChild {
    id: u8,
    control: Arc<Control>,
}
impl Feature for FaultChild {
    type State = usize;
    type Action = DetailAction;
    fn reduce(
        &mut self,
        _: &mut usize,
        _: &DetailAction,
        _: &mut FeatureContext<'_, usize, DetailAction>,
    ) {
    }
}
impl Drop for FaultChild {
    fn drop(&mut self) {
        self.control.visit("child drop", self.id);
    }
}
type FaultState = KeyedState<Key, FaultRow>;

/// Builds the real keyed declaration used by private reconciliation protocol tests.
/// The explicit registry and ingress binding do not model concurrent actor scheduling.
fn owner(
    control: Arc<Control>,
    arm_hash_after_make: bool,
    child_count: u8,
) -> Box<dyn MountDeclaration<FaultState, ()> + Send> {
    let make_capture = Capture("make drop", 0, std::sync::Arc::clone(&control.trace));
    Box::new(crate::store::mount::children::keyed(
        OptionalPath::new(|state: &FaultState| Some(state), |state| Some(state)),
        |state| state,
        |state| state,
        0,
        move |key: &Arc<Key>, _: &FaultRow, _| {
            let _ = &make_capture;
            let id = key.0;
            control.visit("make", id);
            let config_capture = Capture("config drop", id, std::sync::Arc::clone(&control.trace));
            let initial_capture =
                Capture("initial drop", id, std::sync::Arc::clone(&control.trace));
            let config = std::sync::Arc::clone(&control);
            let initial = std::sync::Arc::clone(&control);
            let entry = EntryMount::new(FaultFeature {
                id,
                control: std::sync::Arc::clone(&control),
                _capture: Capture("payload drop", id, std::sync::Arc::clone(&control.trace)),
            })
            .configuration(move |_| {
                let _ = &config_capture;
                config.visit("config", id);
                0
            })
            .on_initial(
                move |_| {
                    let _ = &initial_capture;
                    initial.visit("initial", id);
                },
                |_, _| Task::run(|_| async {}),
            );
            let recipe_capture = RecipeCapture(id, std::sync::Arc::clone(&control));
            let recipe = std::sync::Arc::clone(&control);
            if arm_hash_after_make && id == 3 {
                *control.fault.lock() = Some(("hash", 1, Fault::Close));
            }
            (
                entry,
                Some(
                    move |_: &FaultRow,
                          children: &mut crate::store::mount::children::MountChildren<
                        '_,
                        FaultState,
                        (),
                        FaultRow,
                        EntryAction,
                    >| {
                        let _ = &recipe_capture;
                        recipe.visit("recipe", id);
                        for ordinal in 0..child_count {
                            let child = std::sync::Arc::clone(&recipe);
                            let child_id = id + 10 * ordinal;
                            children.optional(OptionalMount::new(
                                |row: &FaultRow| row.child.as_ref(),
                                |row: &mut FaultRow| row.child.as_mut(),
                                |row: &FaultRow| {
                                    row.child.map(|_| MountIdentity {
                                        key: (),
                                        replacement: 0,
                                        configuration: 0,
                                    })
                                },
                                move |_| {
                                    child.visit("child make", child_id);
                                    FaultChild {
                                        id: child_id,
                                        control: std::sync::Arc::clone(&child),
                                    }
                                },
                            ));
                        }
                    },
                ),
            )
        },
    ))
}

#[test]
fn partial_reorder_preserves_subtrees_but_remounts_stale_boundary_entries() {
    for stale_id in [None, Some(1_u8), Some(6)] {
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        let trace = Trace::default();
        let control = Arc::new(Control {
            fault: Mutex::new(None),
            lifecycle: Arc::clone(&lifecycle),
            trace: Arc::clone(&trace),
        });
        let mut state: FaultState = (1..=6)
            .map(|id| (Key(id, Arc::clone(&control)), FaultRow { child: Some(0) }))
            .collect();
        let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        let ingress = Arc::new(CommandIngress::<Command<FaultState, ()>>::new(
            sender,
            ControlWake::new(),
        ));
        let declaration = owner(Arc::clone(&control), false, 2);
        declaration.bind(&ingress, &lifecycle);
        let mut mounts = vec![declaration];
        let mut nodes = Vec::new();
        crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
            &mut mounts,
            true,
            &state,
            &mut scopes,
            &mut ScopeTransitionBatch::default(),
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
        let previous: Vec<_> = mounts
            .iter()
            .filter_map(|mount| mount.installed_scope())
            .collect();
        let stamps: Vec<_> = mounts
            .iter()
            .filter_map(|mount| mount.position().entry_stamp.cloned())
            .collect();
        assert_eq!(previous.len(), 18);
        assert_eq!(stamps.len(), 6);
        let mut transitions = ScopeTransitionBatch::default();
        if let Some(id) = stale_id {
            transitions.extend(scopes.leave(previous[usize::from(id - 1) * 3]));
        }
        state.move_index(2, 3);
        trace.lock().clear();
        crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
            &mut mounts,
            true,
            &state,
            &mut scopes,
            &mut transitions,
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
        nodes.retain(|node| scopes.is_current(node.scope()));
        let current: Vec<_> = mounts
            .iter()
            .filter_map(|mount| mount.installed_scope())
            .collect();
        let current_stamps: Vec<_> = mounts
            .iter()
            .filter_map(|mount| mount.position().entry_stamp)
            .collect();
        assert_eq!(current.len(), 18);
        assert_eq!(current_stamps.len(), 6);
        assert!(state.iter().map(|(key, _)| key.0).eq([1, 2, 4, 3, 5, 6]));
        for (index, id) in [1_u8, 2, 4, 3, 5, 6].into_iter().enumerate() {
            let old_index = usize::from(id - 1);
            let old = &previous[old_index * 3..old_index * 3 + 3];
            let new = &current[index * 3..index * 3 + 3];
            assert!(current_stamps[index] == &stamps[old_index]);
            assert!(new.iter().all(|token| scopes.is_current(*token)));
            if stale_id == Some(id) {
                assert!(old.iter().all(|token| !scopes.is_current(*token)));
                assert!(old.iter().all(|token| transitions.contains(*token)));
                assert!(new.iter().all(|token| !previous.contains(token)));
            } else {
                assert_eq!(new, old, "the complete retained subtree keeps its scopes");
            }
        }
        let events = trace.lock().clone();
        assert_eq!(
            events
                .iter()
                .filter(|(event, _)| *event == "make")
                .map(|(_, id)| *id)
                .collect::<Vec<_>>(),
            stale_id.into_iter().collect::<Vec<_>>()
        );
        if let Some(id) = stale_id {
            let position = |event| events.iter().position(|item| *item == event).unwrap();
            assert!(position(("child drop", id + 10)) < position(("child drop", id)));
            assert!(position(("child drop", id)) < position(("feature drop", id)));
            assert!(position(("feature drop", id)) < position(("make", id)));
        } else {
            assert!(!events.iter().any(|(event, _)| event.ends_with("drop")));
        }
    }
}

#[test]
fn close_and_panic_in_clone_make_config_and_recipe_stop_the_batch_and_drop_payloads_once() {
    for (phase, key, stop) in [
        ("clone", 2, 3),
        ("make", 2, 4),
        ("config", 1, 7),
        ("recipe", 1, 8),
    ] {
        for fault in [Fault::Close, Fault::Panic] {
            let (lifecycle, mut scopes) = ScopeRegistry::for_test();
            let trace = Trace::default();
            let control = Arc::new(Control {
                fault: Mutex::new(None),
                lifecycle: std::sync::Arc::clone(&lifecycle),
                trace: std::sync::Arc::clone(&trace),
            });
            let state: FaultState = (1..=3)
                .map(|id| {
                    (
                        Key(id, std::sync::Arc::clone(&control)),
                        FaultRow { child: Some(0) },
                    )
                })
                .collect();
            trace.lock().clear();
            *control.fault.lock() = Some((phase, key, fault));
            let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
            let ingress = Arc::new(CommandIngress::<Command<FaultState, ()>>::new(
                sender,
                ControlWake::new(),
            ));
            let declaration = owner(std::sync::Arc::clone(&control), false, 1);
            declaration.bind(&ingress, &lifecycle);
            let mut mounts = vec![declaration];
            let mut nodes = Vec::new();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
                    &mut mounts,
                    true,
                    &state,
                    &mut scopes,
                    &mut ScopeTransitionBatch::default(),
                    &mut nodes,
                    #[cfg(feature = "diagnostics")]
                    None,
                );
            }));
            assert_eq!(
                outcome.is_err(),
                matches!(fault, Fault::Panic),
                "{phase} {fault:?}"
            );
            drop((mounts, nodes));
            let events = trace.lock().clone();
            let callback_events: Vec<_> = events
                .iter()
                .filter(|(event, _)| !event.ends_with("drop"))
                .copied()
                .collect();
            let expected = [
                ("clone", 1),
                ("make", 1),
                ("clone", 2),
                ("make", 2),
                ("clone", 3),
                ("make", 3),
                ("config", 1),
                ("recipe", 1),
            ];
            assert_eq!(callback_events, expected[..stop], "{phase} {fault:?}");
            let constructed = if phase == "recipe" || phase == "config" {
                3
            } else if phase == "make" && matches!(fault, Fault::Close) {
                2
            } else {
                1
            };
            for id in 1..=3 {
                for event in [
                    "feature drop",
                    "payload drop",
                    "config drop",
                    "initial drop",
                    "recipe drop",
                ] {
                    assert_eq!(
                        events.iter().filter(|item| **item == (event, id)).count(),
                        usize::from(id <= constructed),
                        "{phase} {fault:?} {event} {id}"
                    );
                }
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|item| **item == ("make drop", 0))
                    .count(),
                1
            );
        }
    }
}

#[test]
fn keyed_lookup_hash_shutdown_skips_configuration_and_recipe() {
    let (lifecycle, mut scopes) = ScopeRegistry::for_test();
    let trace = Trace::default();
    let control = Arc::new(Control {
        fault: Mutex::new(None),
        lifecycle: std::sync::Arc::clone(&lifecycle),
        trace: std::sync::Arc::clone(&trace),
    });
    let state: FaultState = (1..=3)
        .map(|id| {
            (
                Key(id, std::sync::Arc::clone(&control)),
                FaultRow { child: Some(0) },
            )
        })
        .collect();
    trace.lock().clear();
    let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<FaultState, ()>>::new(
        sender,
        ControlWake::new(),
    ));
    let declaration = owner(std::sync::Arc::clone(&control), true, 1);
    declaration.bind(&ingress, &lifecycle);
    let mut mounts = vec![declaration];
    let mut nodes = Vec::new();
    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
        &mut mounts,
        true,
        &state,
        &mut scopes,
        &mut ScopeTransitionBatch::default(),
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    assert!(lifecycle.is_closed());
    assert!(nodes.is_empty(), "closed identity installs no entry");
    drop((mounts, nodes));
    let events = trace.lock().clone();
    let callbacks: Vec<_> = events
        .iter()
        .filter(|(event, _)| !event.ends_with("drop"))
        .copied()
        .collect();
    assert_eq!(
        callbacks,
        [
            ("clone", 1),
            ("make", 1),
            ("clone", 2),
            ("make", 2),
            ("clone", 3),
            ("make", 3),
            ("hash", 1),
        ]
    );
    for id in 1..=3 {
        for event in [
            "feature drop",
            "payload drop",
            "config drop",
            "initial drop",
            "recipe drop",
        ] {
            assert_eq!(
                events.iter().filter(|item| **item == (event, id)).count(),
                1,
                "{event} {id}"
            );
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|item| **item == ("make drop", 0))
            .count(),
        1
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One retirement schedule contrasts close and panic in owned descendants while tracking retained siblings"
)]
fn old_descendant_destructor_close_drains_departures_but_never_makes_successors() {
    for preserve_edges in [false, true] {
        for phase in ["child drop", "recipe drop", "key drop"] {
            for fault in [Fault::Close, Fault::Panic] {
                let (lifecycle, mut scopes) = ScopeRegistry::for_test();
                let trace = Trace::default();
                let control = Arc::new(Control {
                    fault: Mutex::new(None),
                    lifecycle: std::sync::Arc::clone(&lifecycle),
                    trace: std::sync::Arc::clone(&trace),
                });
                let initial = if preserve_edges {
                    &[0, 1, 2, 3, 6][..]
                } else {
                    &[1, 2, 3][..]
                };
                let mut state: FaultState = initial
                    .iter()
                    .copied()
                    .map(|id| {
                        (
                            Key(id, std::sync::Arc::clone(&control)),
                            FaultRow { child: Some(0) },
                        )
                    })
                    .collect();
                let (sender, _receiver) =
                    crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
                let ingress = Arc::new(CommandIngress::<Command<FaultState, ()>>::new(
                    sender,
                    ControlWake::new(),
                ));
                let declaration = owner(std::sync::Arc::clone(&control), false, 1);
                declaration.bind(&ingress, &lifecycle);
                let mut mounts = vec![declaration];
                let mut nodes = Vec::new();
                crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
                    &mut mounts,
                    true,
                    &state,
                    &mut scopes,
                    &mut ScopeTransitionBatch::default(),
                    &mut nodes,
                    #[cfg(feature = "diagnostics")]
                    None,
                );
                let retained: Vec<_> = mounts
                    .iter()
                    .filter_map(|mount| mount.installed_scope())
                    .enumerate()
                    .filter(|(index, _)| {
                        if preserve_edges {
                            *index < 2 || *index >= 6
                        } else {
                            *index >= 4
                        }
                    })
                    .map(|(_, token)| token)
                    .collect();
                assert_eq!(retained.len(), if preserve_edges { 6 } else { 2 });
                for id in [1, 2] {
                    state.remove(&Key(id, Arc::clone(&control)));
                }
                for id in [4, 5] {
                    state.insert(Key(id, Arc::clone(&control)), FaultRow { child: Some(0) });
                    if preserve_edges {
                        state.move_index(state.len() - 1, state.len() - 2);
                    }
                }
                if preserve_edges {
                    assert!(state.iter().map(|(key, _)| key.0).eq([0, 3, 4, 5, 6]));
                    let mut stamps = mounts
                        .iter()
                        .filter_map(|mount| mount.position().entry_stamp);
                    assert!(
                        stamps
                            .next()
                            .unwrap()
                            .matches(state.entry_stamp(&Key(0, Arc::clone(&control))).unwrap())
                    );
                    assert!(
                        stamps
                            .next_back()
                            .unwrap()
                            .matches(state.entry_stamp(&Key(6, Arc::clone(&control))).unwrap())
                    );
                }
                trace.lock().clear();
                *control.fault.lock() = Some((phase, 2, fault));
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
                        &mut mounts,
                        true,
                        &state,
                        &mut scopes,
                        &mut ScopeTransitionBatch::default(),
                        &mut nodes,
                        #[cfg(feature = "diagnostics")]
                        None,
                    );
                }));
                assert_eq!(outcome.is_err(), matches!(fault, Fault::Panic), "{phase}");
                let events = trace.lock().clone();
                assert!(
                    events.iter().all(|(event, _)| event.ends_with("drop")),
                    "no successor callback after retirement interruption: {phase} {fault:?} {events:?}"
                );
                for id in [1, 2] {
                    let child = events
                        .iter()
                        .position(|item| *item == ("child drop", id))
                        .unwrap();
                    let parent = events
                        .iter()
                        .position(|item| *item == ("feature drop", id))
                        .unwrap();
                    if matches!(fault, Fault::Close) || id == 2 {
                        assert!(child < parent, "{phase} {fault:?} {id}");
                    }
                    for event in [
                        "child drop",
                        "feature drop",
                        "payload drop",
                        "config drop",
                        "initial drop",
                        "recipe drop",
                    ] {
                        assert_eq!(
                            events.iter().filter(|item| **item == (event, id)).count(),
                            1,
                            "{phase} {fault:?} {event} {id}"
                        );
                    }
                }
                if matches!(fault, Fault::Close) {
                    assert!(lifecycle.is_closed());
                    for token in retained {
                        assert!(
                            scopes.is_current(token),
                            "root closure did not selectively retire the retained sibling"
                        );
                    }
                }
                // Root closure or unwinding drops both the retained-subtree map
                // and any preserved prefix/suffix. None stays usable after shutdown.
                let interrupted = events.iter().position(|item| *item == (phase, 2)).unwrap();
                let retained_ids = if preserve_edges {
                    &[0, 3, 6][..]
                } else {
                    &[3][..]
                };
                for retained_id in retained_ids {
                    let sibling_disposal =
                        events.iter().position(|(_, id)| id == retained_id).unwrap();
                    assert!(
                        sibling_disposal > interrupted,
                        "retained sibling was disposed before the interruption: {phase} {fault:?}"
                    );
                    for event in [
                        "child drop",
                        "feature drop",
                        "payload drop",
                        "config drop",
                        "initial drop",
                        "recipe drop",
                    ] {
                        assert_eq!(
                            events
                                .iter()
                                .filter(|item| **item == (event, *retained_id))
                                .count(),
                            1,
                            "{phase} {fault:?} retained sibling {retained_id} {event} before fixture cleanup"
                        );
                    }
                }
                drop((mounts, nodes));
                let events = trace.lock();
                for id in retained_ids {
                    for event in [
                        "child drop",
                        "feature drop",
                        "payload drop",
                        "config drop",
                        "initial drop",
                        "recipe drop",
                    ] {
                        assert_eq!(
                            events.iter().filter(|item| **item == (event, *id)).count(),
                            1,
                            "{phase} {fault:?} retained sibling {id} {event}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One keyed retirement proves authority invalidation and exact ownership disposal across a partially consumed subtree iterator"
)]
fn last_descendant_panic_drains_remaining_tail_head_and_other_subtrees_once() {
    let (lifecycle, mut scopes) = ScopeRegistry::for_test();
    let trace = Trace::default();
    let control = Arc::new(Control {
        fault: Mutex::new(None),
        lifecycle: Arc::clone(&lifecycle),
        trace: Arc::clone(&trace),
    });
    let mut state: FaultState = (1..=3)
        .map(|id| (Key(id, Arc::clone(&control)), FaultRow { child: Some(0) }))
        .collect();
    let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<FaultState, ()>>::new(
        sender,
        ControlWake::new(),
    ));
    let declaration = owner(Arc::clone(&control), false, 2);
    declaration.bind(&ingress, &lifecycle);
    let mut mounts = vec![declaration];
    let mut nodes = Vec::new();
    crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
        &mut mounts,
        true,
        &state,
        &mut scopes,
        &mut ScopeTransitionBatch::default(),
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    let installed: Vec<_> = mounts
        .iter()
        .filter_map(|mount| mount.installed_scope())
        .collect();
    assert_eq!(
        installed.len(),
        9,
        "each entry has two independently owned descendants"
    );
    let departed = &installed[..6];
    for (index, token) in (1..=6).zip(departed) {
        // Registry invalidation removes each record before dropping its cache.
        scopes.put_scope_cache(
            *token,
            Box::new(Capture("authority drop", index, Arc::clone(&trace))),
        );
    }
    for id in [1, 2] {
        state.remove(&Key(id, Arc::clone(&control)));
    }
    for id in [4, 5] {
        state.insert(Key(id, Arc::clone(&control)), FaultRow { child: Some(0) });
    }
    trace.lock().clear();
    // Descendants are destroyed in reverse declaration order. Entry2's last
    // child panics while its first child and head are still iterator-owned.
    *control.fault.lock() = Some(("child drop", 12, Fault::Panic));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::store::mount::nested::reconcile_roster::<_, (), Shared>(
            &mut mounts,
            true,
            &state,
            &mut scopes,
            &mut ScopeTransitionBatch::default(),
            &mut nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
    }));
    assert!(outcome.is_err());
    assert!(departed.iter().all(|token| !scopes.is_current(*token)));
    let events = trace.lock().clone();
    let interrupted = events
        .iter()
        .position(|item| *item == ("child drop", 12))
        .unwrap();
    assert_eq!(
        events
            .iter()
            .find(|(event, _)| *event == "child drop" || *event == "feature drop"),
        Some(&("child drop", 12)),
        "the last descendant is the first owned feature destructor"
    );
    for index in 1..=6 {
        assert_eq!(
            events[..interrupted]
                .iter()
                .filter(|item| **item == ("authority drop", index))
                .count(),
            1
        );
    }
    assert!(
        events.iter().all(|(event, _)| event.ends_with("drop")),
        "no successor callback after the destructor panic: {events:?}"
    );
    // These owners must be gone when unwinding returns, not merely released by
    // the fixture's explicit roster cleanup below.
    for id in 1..=2 {
        for child in [id, id + 10] {
            assert_eq!(
                events
                    .iter()
                    .filter(|item| **item == ("child drop", child))
                    .count(),
                1,
                "child {child}"
            );
        }
        for event in [
            "feature drop",
            "payload drop",
            "config drop",
            "initial drop",
            "recipe drop",
        ] {
            assert_eq!(
                events.iter().filter(|item| **item == (event, id)).count(),
                1,
                "{event} {id}"
            );
        }
    }
    for item in [("child drop", 2), ("feature drop", 2)] {
        assert!(
            events.iter().position(|event| *event == item).unwrap() > interrupted,
            "remaining tail and head must be disposed during unwinding: {item:?}"
        );
    }
    drop((mounts, nodes));
    let events = trace.lock();
    for id in 1..=3 {
        for item in [
            ("child drop", id),
            ("child drop", id + 10),
            ("feature drop", id),
            ("payload drop", id),
            ("config drop", id),
            ("initial drop", id),
            ("recipe drop", id),
        ] {
            assert_eq!(
                events.iter().filter(|event| **event == item).count(),
                1,
                "exactly once after final cleanup: {item:?}"
            );
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|item| **item == ("make drop", 0))
            .count(),
        1
    );
}
