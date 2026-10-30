use super::{poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::{Effect, Feature, FeatureContext, Reduce, RootMount, Runtime, Store};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

#[derive(Default)]
struct Events {
    values: [Option<LifecycleEvent>; 16],
    length: usize,
}

/// Separates root lifecycle facts, reducer entry and State disposal.
#[derive(Default)]
struct Probe {
    events: Mutex<Events>,
    reductions: AtomicUsize,
    state_drops: AtomicUsize,
}

impl Probe {
    /// Copies one sink event into the fixed buffer without allocating.
    fn record(&self, event: &LifecycleEvent) {
        let mut events = self.events.lock().unwrap();
        let index = events.length;
        events.values[index] = Some(*event);
        events.length += 1;
    }

    /// Collects recorded events outside the sink callback, then resets the buffer.
    fn take(&self) -> Vec<LifecycleEvent> {
        let mut events = self.events.lock().unwrap();
        let values = events.values[..events.length]
            .iter()
            .map(|event| event.unwrap())
            .collect();
        *events = Events::default();
        values
    }
}

// No Clone, Debug or Sync: diagnostics must not add State bounds.
struct State {
    probe: Arc<Probe>,
    _not_sync: Cell<()>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.probe.state_drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Root;

impl Feature for Root {
    type State = State;
    type Action = ();

    fn update(&mut self, state: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {
        state.probe.reductions.fetch_add(1, Ordering::SeqCst);
    }
}

/// Selects the three public root-builder paths without changing the fixture.
#[derive(Clone, Copy)]
enum Surface {
    Legacy,
    Feature,
    NativeRoot,
}

struct CaptureDrop(Arc<AtomicUsize>);
impl Drop for CaptureDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn replacing_builder_sink_and_failed_build_drop_captures_without_invocation() {
    for feature in [false, true] {
        let runtime = support::ManualRuntime::new();
        let first_drops = Arc::new(AtomicUsize::new(0));
        let last_drops = Arc::new(AtomicUsize::new(0));
        let first = CaptureDrop(Arc::clone(&first_drops));
        let last = CaptureDrop(Arc::clone(&last_drops));
        let first_sink = move |_: &LifecycleEvent| {
            let _ = &first;
            panic!("builder replacement must not invoke the old sink");
        };
        let last_sink = move |_: &LifecycleEvent| {
            let _ = &last;
            panic!("failed construction must not invoke its sink");
        };
        let probe = Arc::new(Probe::default());
        let state = State {
            probe: Arc::clone(&probe),
            _not_sync: Cell::new(()),
        };
        let failed = if feature {
            let builder = Store::builder(state, Root, runtime.runtime())
                .with_lifecycle_diagnostics(first_sink)
                .with_lifecycle_diagnostics(last_sink);
            assert_eq!(first_drops.load(Ordering::SeqCst), 1);
            assert_eq!(last_drops.load(Ordering::SeqCst), 0);
            builder.mailbox_capacity(0).build().is_err()
        } else {
            let builder = Store::builder(
                state,
                Reduce::new(|_: &mut State, (): &(), _| Effect::none()),
                runtime.runtime(),
            )
            .with_lifecycle_diagnostics(first_sink)
            .with_lifecycle_diagnostics(last_sink);
            assert_eq!(first_drops.load(Ordering::SeqCst), 1);
            assert_eq!(last_drops.load(Ordering::SeqCst), 0);
            builder.mailbox_capacity(0).build().is_err()
        };
        assert!(failed);
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);
        assert_eq!(last_drops.load(Ordering::SeqCst), 1);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.spawn_count(), 0);
    }
}

/// Builds one root surface with a move-only, non-Sync State and supplied sink.
fn build(
    surface: Surface,
    runtime: Runtime,
    probe: Arc<Probe>,
    sink: impl FnMut(&LifecycleEvent) + Send + 'static,
) -> Store<State, ()> {
    let state = State {
        probe,
        _not_sync: Cell::new(()),
    };
    match surface {
        Surface::Legacy => Store::builder(
            state,
            Reduce::new(|state: &mut State, (): &(), _| {
                state.probe.reductions.fetch_add(1, Ordering::SeqCst);
                Effect::none()
            }),
            runtime,
        )
        .with_lifecycle_diagnostics(sink)
        .build()
        .unwrap(),
        Surface::Feature => Store::builder(state, Root, runtime)
            .with_lifecycle_diagnostics(sink)
            .build()
            .unwrap(),
        Surface::NativeRoot => RootMount::new(Root)
            .builder(state, runtime)
            .with_lifecycle_diagnostics(sink)
            .build()
            .unwrap(),
    }
}

/// Returns the exact normal actor-stop event sequence.
fn terminal() -> [LifecycleKind; 3] {
    [
        LifecycleKind::IngressClosed,
        LifecycleKind::CancellationRequested {
            cause: CancellationCause::ActorStop,
            scope: None,
        },
        LifecycleKind::ActorStopped,
    ]
}

// Catches root delivery before a pending native Structural transaction is
// accepted, including closure while that first transaction is held by a test.
#[cfg(feature = "test-util")]
#[test]
fn closure_during_gated_native_installation_emits_no_installation_fact() {
    let runtime = support::ManualRuntime::new();
    let gate = rca_core::testing::TransactionGate::new();
    let probe = Arc::new(Probe::default());
    let sink = Arc::clone(&probe);
    let store = RootMount::new(Root)
        .builder(
            State {
                probe: Arc::clone(&probe),
                _not_sync: Cell::new(()),
            },
            runtime.runtime(),
        )
        .transaction_gate(gate.clone())
        .with_lifecycle_diagnostics(move |event| sink.record(event))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    assert!(gate.has_pending());
    assert!(probe.take().is_empty());
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(!gate.has_pending());
    let stopped = probe.take();
    assert_eq!(
        stopped.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
        terminal()
    );
    assert!(
        stopped
            .iter()
            .all(|event| event.last_committed_version() == 0)
    );
    assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
    assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn destroying_an_unpolled_actor_never_reports_root_installation_or_terminal_events() {
    for surface in [Surface::Legacy, Surface::Feature, Surface::NativeRoot] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let sink = Arc::clone(&probe);
        let store = build(
            surface,
            runtime.runtime(),
            Arc::clone(&probe),
            move |event| {
                sink.record(event);
            },
        );
        assert!(probe.take().is_empty());
        let actor = runtime.take_only_queued_task();
        assert!(probe.take().is_empty());
        drop(actor);
        assert!(probe.take().is_empty());
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
        drop(store);
        assert!(probe.take().is_empty());
    }
}

#[test]
fn closing_store_before_first_poll_never_reports_root_installation() {
    for surface in [Surface::Legacy, Surface::Feature, Surface::NativeRoot] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let sink = Arc::clone(&probe);
        let store = build(
            surface,
            runtime.runtime(),
            Arc::clone(&probe),
            move |event| {
                sink.record(event);
            },
        );
        let mut actor = runtime.take_only_queued_task();
        drop(store);
        assert!(probe.take().is_empty());
        assert!(poll(actor.as_mut()).is_ready());
        let stopped = probe.take();
        assert_eq!(
            stopped.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
            terminal()
        );
        assert!(
            stopped
                .iter()
                .all(|event| event.last_committed_version() == 0)
        );
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn closing_during_lifecycle_notification_never_installs_root() {
    use rca_core::RuntimeFuture;
    use std::task::{Context, Wake, Waker};

    struct PollActorOnClose {
        actor: Mutex<Option<RuntimeFuture>>,
    }

    impl Wake for PollActorOnClose {
        fn wake(self: Arc<Self>) {
            let mut actor = self.actor.lock().unwrap().take().unwrap();
            // Release the fixture lock before the actor invokes lifecycle callbacks.
            assert!(poll(actor.as_mut()).is_ready());
        }
    }

    for surface in [Surface::Legacy, Surface::Feature] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let sink = Arc::clone(&probe);
        let store = build(
            surface,
            runtime.runtime(),
            Arc::clone(&probe),
            move |event| {
                sink.record(event);
            },
        );
        let wake = Arc::new(PollActorOnClose {
            actor: Mutex::new(Some(runtime.take_only_queued_task())),
        });
        let waker = Waker::from(Arc::clone(&wake));
        let mut closed = Box::pin(store.closed());
        assert!(
            closed
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert!(probe.take().is_empty());
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 0);

        // Closure notification polls the actor before Store::drop closes its ingress.
        drop(store);

        assert!(wake.actor.lock().unwrap().is_none());
        assert!(poll(closed.as_mut()).is_ready());
        let stopped = probe.take();
        assert_eq!(
            stopped.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
            terminal()
        );
        assert!(
            stopped
                .iter()
                .all(|event| event.last_committed_version() == 0)
        );
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn mount_free_default_builders_install_root_only_on_the_first_live_poll_at_revision_zero() {
    for surface in [Surface::Legacy, Surface::Feature] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let sink = Arc::clone(&probe);
        let store = build(
            surface,
            runtime.runtime(),
            Arc::clone(&probe),
            move |event| {
                sink.record(event);
            },
        );
        let mut actor = runtime.take_only_queued_task();
        assert!(probe.take().is_empty());
        assert!(poll(actor.as_mut()).is_pending());
        let installed = probe.take();
        assert_eq!(installed.len(), 1);
        assert_eq!(
            installed[0].kind(),
            LifecycleKind::Installed {
                lifetime: 0,
                parent: None,
                configuration: Some(0),
            }
        );
        assert_eq!(installed[0].last_committed_version(), 0);
        assert!(poll(actor.as_mut()).is_pending());
        assert!(probe.take().is_empty());
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
        let stopped = probe.take();
        assert_eq!(
            stopped.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
            terminal()
        );
        assert!(
            stopped
                .iter()
                .all(|event| event.last_committed_version() == 0)
        );
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn shutdown_admitted_at_the_first_head_suppresses_root_installation_on_all_builder_surfaces() {
    for surface in [Surface::Legacy, Surface::Feature, Surface::NativeRoot] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let sink = Arc::clone(&probe);
        let store = build(
            surface,
            runtime.runtime(),
            Arc::clone(&probe),
            move |event| {
                sink.record(event);
            },
        );
        let mut actor = runtime.take_only_queued_task();
        let mut shutdown = Box::pin(store.shutdown());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert!(probe.take().is_empty());
        assert!(poll(actor.as_mut()).is_ready());
        assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
        let stopped = probe.take();
        assert_eq!(
            stopped.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
            terminal()
        );
        assert!(
            stopped
                .iter()
                .all(|event| event.last_committed_version() == 0)
        );
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn installed_callback_closing_root_rejects_the_retained_head_without_reducer_entry() {
    for surface in [Surface::Legacy, Surface::Feature] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let owner = Arc::new(Mutex::new(None::<Store<State, ()>>));
        let closing = Arc::clone(&owner);
        let sink = Arc::clone(&probe);
        let store = build(
            surface,
            runtime.runtime(),
            Arc::clone(&probe),
            move |event| {
                sink.record(event);
                if matches!(event.kind(), LifecycleKind::Installed { .. }) {
                    // Release the caller's slot lock before the final owner's Drop.
                    let owner = closing.lock().unwrap().take();
                    drop(owner);
                }
            },
        );
        let receipt = store.try_send(()).unwrap();
        *owner.lock().unwrap() = Some(store);
        let mut actor = runtime.take_only_queued_task();
        assert!(probe.take().is_empty());
        assert!(poll(actor.as_mut()).is_ready());
        assert!(owner.lock().unwrap().is_none());
        assert!(matches!(
            poll(Box::pin(receipt.finish()).as_mut()),
            Poll::Ready(Err(_))
        ));
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
        let events = probe.take();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0].kind(),
            LifecycleKind::Installed {
                lifetime: 0,
                parent: None,
                configuration: Some(0),
            }
        );
        assert_eq!(
            events[1..]
                .iter()
                .map(LifecycleEvent::kind)
                .collect::<Vec<_>>(),
            terminal()
        );
        assert!(
            events
                .iter()
                .all(|event| event.last_committed_version() == 0)
        );
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one two-boundary shutdown schedule keeps selector ordering and incarnation disposal visible"
)]
fn keyed_selectors_closing_root_stop_reconciliation_before_retiring_or_reconstructing_rows() {
    use rca_core::{EntryMount, KeyedState, MountIdentity, OptionalMount};
    use std::sync::atomic::AtomicBool;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Selector {
        Path,
        Collection,
    }
    struct Control {
        selector: Selector,
        armed: AtomicBool,
        closed: AtomicBool,
        owner: Mutex<Option<Store<ClosingState, ()>>>,
        paths: AtomicUsize,
        collections: AtomicUsize,
        makes: AtomicUsize,
        final_row: AtomicUsize,
        probe: Arc<Probe>,
    }
    impl Control {
        fn enter(&self, selector: Selector) {
            assert!(
                !self.closed.load(Ordering::SeqCst),
                "no selector runs after the final owner closes"
            );
            match selector {
                Selector::Path => self.paths.fetch_add(1, Ordering::SeqCst),
                Selector::Collection => self.collections.fetch_add(1, Ordering::SeqCst),
            };
            if self.armed.load(Ordering::SeqCst) && self.selector == selector {
                self.closed.store(true, Ordering::SeqCst);
                let owner = self
                    .owner
                    .lock()
                    .unwrap()
                    .take()
                    .expect("selector owns the final Store");
                drop(owner);
            }
        }
    }
    struct ClosingState {
        panel: Option<PanelState>,
        control: Arc<Control>,
    }
    impl Drop for ClosingState {
        fn drop(&mut self) {
            self.control.final_row.store(
                usize::from(*self.panel.as_ref().unwrap().rows.get(&2).unwrap_or(&0)),
                Ordering::SeqCst,
            );
            self.control
                .probe
                .state_drops
                .fetch_add(1, Ordering::SeqCst);
        }
    }
    struct PanelState {
        rows: KeyedState<u8, u8>,
        control: Arc<Control>,
    }
    struct Replace;
    impl Feature for Replace {
        type State = ClosingState;
        type Action = ();
        fn update(
            &mut self,
            state: &mut ClosingState,
            (): &(),
            _: &mut FeatureContext<'_, ClosingState, ()>,
        ) {
            state
                .control
                .probe
                .reductions
                .fetch_add(1, Ordering::SeqCst);
            let rows = &mut state.panel.as_mut().unwrap().rows;
            assert_eq!(rows.remove(&1), Some(7));
            rows.insert(2, 8);
            state.control.armed.store(true, Ordering::SeqCst);
        }
    }
    struct Panel {
        _drop: CaptureDrop,
    }
    impl Feature for Panel {
        type State = PanelState;
        type Action = ();
        fn update(
            &mut self,
            _: &mut PanelState,
            (): &(),
            _: &mut FeatureContext<'_, PanelState, ()>,
        ) {
            panic!("root replacement must not reduce the panel");
        }
    }
    struct Row {
        _drop: CaptureDrop,
    }
    impl Feature for Row {
        type State = u8;
        type Action = ();
        fn update(&mut self, _: &mut u8, (): &(), _: &mut FeatureContext<'_, u8, ()>) {
            panic!("root replacement must not reduce a row");
        }
    }

    for selector in [Selector::Path, Selector::Collection] {
        let runtime = support::ManualRuntime::new();
        let probe = Arc::new(Probe::default());
        let control = Arc::new(Control {
            selector,
            armed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            owner: Mutex::new(None),
            paths: AtomicUsize::new(0),
            collections: AtomicUsize::new(0),
            makes: AtomicUsize::new(0),
            final_row: AtomicUsize::new(0),
            probe: Arc::clone(&probe),
        });
        let feature_drops = Arc::new(AtomicUsize::new(0));
        let capture_drops = Arc::new(AtomicUsize::new(0));
        let parent_drop = Arc::clone(&feature_drops);
        let parent_capture = CaptureDrop(Arc::clone(&capture_drops));
        let panel = OptionalMount::new(
            |state: &ClosingState| {
                state.control.enter(Selector::Path);
                state.panel.as_ref()
            },
            |state: &mut ClosingState| state.panel.as_mut(),
            |_| {
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                let _ = &parent_capture;
                Panel {
                    _drop: CaptureDrop(Arc::clone(&parent_drop)),
                }
            },
        );
        let made = Arc::clone(&control);
        let row_drop = Arc::clone(&feature_drops);
        let row_capture_drops = Arc::clone(&capture_drops);
        let sink = Arc::clone(&probe);
        let (builder, _) = Store::builder(
            ClosingState {
                panel: Some(PanelState {
                    rows: [(1, 7)].into_iter().collect(),
                    control: Arc::clone(&control),
                }),
                control: Arc::clone(&control),
            },
            Replace,
            runtime.runtime(),
        )
        .mount_with_children(panel, move |_, children| {
            let made = Arc::clone(&made);
            let row_drop = Arc::clone(&row_drop);
            let row_capture = CaptureDrop(Arc::clone(&row_capture_drops));
            children.keyed(
                |state| {
                    state.control.enter(Selector::Collection);
                    &state.rows
                },
                |state| &mut state.rows,
                move |key, value| {
                    let _ = &row_capture;
                    assert!(
                        !made.closed.load(Ordering::SeqCst),
                        "no construction follows selector closure"
                    );
                    assert_eq!((*key, *value), (1, 7));
                    made.makes.fetch_add(1, Ordering::SeqCst);
                    EntryMount::new(Row {
                        _drop: CaptureDrop(Arc::clone(&row_drop)),
                    })
                },
            );
        });
        let store = builder
            .with_lifecycle_diagnostics(move |event| sink.record(event))
            .build()
            .unwrap();
        let mut actor = runtime.take_only_queued_task();
        assert!(poll(actor.as_mut()).is_pending());
        let installed = probe.take();
        assert_eq!(installed.len(), 3);
        assert!(
            installed
                .iter()
                .all(|event| matches!(event.kind(), LifecycleKind::Installed { .. }))
        );
        assert_eq!(control.makes.load(Ordering::SeqCst), 1);
        assert_eq!(feature_drops.load(Ordering::SeqCst), 0);
        // Installation runs unarmed. The unchanged parent's identity selector does not
        // read its path, so the next path read belongs to collection traversal itself.
        let paths = control.paths.load(Ordering::SeqCst);
        let collections = control.collections.load(Ordering::SeqCst);
        let receipt = store.try_send(()).unwrap();
        *control.owner.lock().unwrap() = Some(store);

        assert!(poll(actor.as_mut()).is_ready());
        assert!(control.owner.lock().unwrap().is_none());
        assert!(control.closed.load(Ordering::SeqCst));
        assert_eq!(control.paths.load(Ordering::SeqCst), paths + 1);
        assert_eq!(
            control.collections.load(Ordering::SeqCst),
            collections + usize::from(selector == Selector::Collection)
        );
        let Poll::Ready(Ok(task)) = poll(Box::pin(receipt.finish()).as_mut()) else {
            panic!("the root mutation committed before selector shutdown");
        };
        assert!(task.is_finished());
        assert_eq!(probe.reductions.load(Ordering::SeqCst), 1);
        assert_eq!(control.makes.load(Ordering::SeqCst), 1);
        assert_eq!(
            probe
                .take()
                .iter()
                .map(LifecycleEvent::kind)
                .collect::<Vec<_>>(),
            terminal()
        );
        drop(actor);
        assert_eq!(control.final_row.load(Ordering::SeqCst), 8);
        assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
        assert_eq!(feature_drops.load(Ordering::SeqCst), 2);
        assert_eq!(capture_drops.load(Ordering::SeqCst), 2);
    }
}
