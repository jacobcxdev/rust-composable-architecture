//! Nominal Rc leaf for the private shared traversal compiler gate.
//!
//! Callback ownership uses local cells, but traversal remains the shared private
//! algorithm. Queries are actor-serial and do not hold cell borrows across callbacks.
//! This fixture is not a public fold API or a concurrent callback registry.
use super::super::nested::fold_gate::{CallbackLease, Contribution, PureEntry, Stats, walk};
use super::*;
use crate::effect::lineage::ControlWake;
use crate::store::ingress::CommandIngress;
use crate::store::output::{FrozenRegistration, local as output};
use std::cell::Cell;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;

/// Keeps local contribution callbacks beside their weak registration and leased route.
struct TypedEntry<R, A, S, X, K: Contribution> {
    /// Reserves declaration identity without retaining its callback payload.
    registration: LocalWeak<dyn output::HandlerRegistration<R, S, X, A>>,
    /// Exists only while the actor-serial query leases this entry.
    route: Option<Box<dyn output::HandlerRoute<R, A, S, X>>>,
    /// Reads local state and returns a value without `Send` requirements.
    #[expect(
        clippy::type_complexity,
        reason = "keep the local contribution callback ownership explicit in the private gate"
    )]
    contribution: Box<dyn Fn(&S) -> K::Value>,
    /// Adds a subtree accumulator only when an explicit transform exists.
    #[expect(
        clippy::type_complexity,
        reason = "keep the optional owned local value transform explicit in the private gate"
    )]
    transform: Option<Box<dyn Fn(K::Value) -> K::Value>>,
}
impl<R, A, S, X, K: Contribution> PureEntry<R, K> for TypedEntry<R, A, S, X, K> {
    fn freeze(&mut self, scopes: &ScopeRegistry) -> bool {
        self.route = self
            .registration
            .upgrade()
            .and_then(|site| site.freeze_handler_route(scopes));
        self.route
            .as_ref()
            .is_some_and(|route| route.registration().is_current(scopes))
    }
    fn contribution(&self, root: &R) -> Option<K::Value> {
        self.route.as_ref()?.read(root).map(&self.contribution)
    }
    fn has_transform(&self) -> bool {
        self.transform.is_some()
    }
    fn transform(&self, value: K::Value) -> K::Value {
        self.transform.as_ref().unwrap()(value)
    }
    fn clear_route(&mut self) {
        self.route = None;
    }
}
/// Transfers callback ownership without keeping the transfer borrow across user code.
struct EntryCell<R, K: Contribution>(
    /// Is empty while the query holds the callback lease.
    RefCell<Option<Box<dyn PureEntry<R, K>>>>,
);
/// Owns a callback cell for the original source installation only.
struct EntryOwner<R, K: Contribution> {
    /// Owns the payload except while an actor-serial query leases it.
    cell: Rc<EntryCell<R, K>>,
    /// Resolves the original source scope without keeping the source alive.
    source: LocalWeak<dyn output::OutputRegistration>,
    /// Retains the first installed parent and never retargets its successor.
    parent: Cell<Option<ScopeToken>>,
    /// Places this callback owner below its source in committed preorder.
    depth: usize,
}
impl<R, K: Contribution> EntryOwner<R, K> {
    /// Takes the payload inside its borrow and destroys captures after releasing it.
    fn drain(&self) {
        let payload = { self.cell.0.borrow_mut().take() };
        drop(payload);
    }
}
impl<R, K: Contribution> Drop for EntryOwner<R, K> {
    fn drop(&mut self) {
        self.drain();
    }
}
impl<R, A, K: Contribution> MountDeclaration<R, A, Local> for EntryOwner<R, K> {
    fn depth(&self) -> usize {
        self.depth
    }
    fn position(&self) -> MountPosition<'_> {
        MountPosition {
            depth: self.depth,
            parent: self.parent.get(),
            entry_stamp: None,
        }
    }
    fn bind(&self, _: &Arc<CommandIngress<Command<R, A, Local>>>, _: &Arc<Lifecycle>) {}
    fn reconcile(
        &self,
        _: &R,
        scopes: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Mount>>,
        _: &mut crate::store::mount::nested::Descendants<'_, R, A, Local>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        if self.parent.get().is_none() {
            self.parent.set(
                self.source
                    .upgrade()
                    .and_then(|source| source.freeze(scopes))
                    .map(|source| source.scope),
            );
        }
    }
}
/// Keeps only weak callback cells indexed by reserved declaration addresses.
#[expect(
    clippy::type_complexity,
    reason = "keep the local weak index ownership explicit in the private gate"
)]
struct Index<R, K: Contribution>(
    /// Shares index metadata without adding strong callback ownership.
    Rc<RefCell<Vec<(usize, LocalWeak<EntryCell<R, K>>)>>>,
);
impl<R, K: Contribution> Clone for Index<R, K> {
    fn clone(&self) -> Self {
        Self(std::rc::Rc::clone(&self.0))
    }
}
impl<R: 'static, K: Contribution> Index<R, K> {
    /// Creates an empty local index shared by the fixture and its recipes.
    fn new() -> Self {
        Self(Rc::new(RefCell::new(Vec::new())))
    }
    /// Adds a local callback owner below its source declaration.
    #[expect(
        clippy::type_complexity,
        reason = "keep the local registration callback and transform ownership explicit"
    )]
    fn register<A: 'static, S: 'static, X: 'static, P>(
        &self,
        children: &mut ChildMounts<'_, R, A, P>,
        site: &LocalMountSite<R, S, X, A>,
        target_depth: usize,
        contribution: impl Fn(&S) -> K::Value + 'static,
        transform: Option<Box<dyn Fn(K::Value) -> K::Value>>,
    ) {
        let registration = site.handler_registration();
        let identity = registration.as_ptr().cast::<()>() as usize;
        let cell = Rc::new(EntryCell(RefCell::new(Some(
            Box::new(TypedEntry::<_, _, _, _, K> {
                registration,
                route: None,
                contribution: Box::new(contribution),
                transform,
            }) as Box<dyn PureEntry<R, K>>,
        ))));
        self.0.borrow_mut().push((identity, Rc::downgrade(&cell)));
        children.discovered.push(Box::new(EntryOwner {
            cell,
            source: site.output_registration(),
            parent: Cell::new(None),
            depth: target_depth + 1,
        }));
    }
    /// Takes an entry outside index and payload borrows, then freezes its route.
    fn acquire(
        &self,
        identity: usize,
        scopes: &ScopeRegistry,
        stats: &mut Stats,
    ) -> Option<Lease<R, K>> {
        let cell = {
            let mut entries = self.0.borrow_mut();
            entries.retain(|(_, cell)| cell.strong_count() != 0);
            entries.iter().find_map(|(current, cell)| {
                stats.comparisons += 1;
                let cell = cell.upgrade()?;
                (*current == identity).then_some(cell)
            })
        }?;
        let payload = { cell.0.borrow_mut().take() }?;
        let mut lease = Lease {
            cell,
            payload: Some(payload),
        };
        if !lease.payload.as_mut().unwrap().freeze(scopes) {
            return None;
        }
        stats.frozen_routes += 1;
        Some(lease)
    }
}
/// Restores callbacks after return or unwind without a live payload borrow.
struct Lease<R, K: Contribution> {
    /// Keeps the restore slot alive until this query releases it.
    cell: Rc<EntryCell<R, K>>,
    /// Owns the callbacks while their cell is empty.
    payload: Option<Box<dyn PureEntry<R, K>>>,
}
impl<R, K: Contribution> CallbackLease<R, K> for Lease<R, K> {
    fn entry(&self) -> &dyn PureEntry<R, K> {
        self.payload.as_deref().unwrap()
    }
}
impl<R, K: Contribution> Drop for Lease<R, K> {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.as_mut() {
            payload.clear_route();
        }
        {
            let mut destination = self.cell.0.borrow_mut();
            std::mem::swap(&mut *destination, &mut self.payload);
        };
        drop(self.payload.take());
    }
}
/// Captures local declaration identity and installation without owning its callbacks.
struct Subtree {
    /// Prevents the captured declaration address from being reused.
    reservation: LocalWeak<dyn output::OutputRegistration>,
    /// Locates the reserved declaration in the committed roster.
    identity: usize,
    /// Rejects stale installation generations and foreign registries.
    frozen: FrozenRegistration,
}
impl Subtree {
    /// Captures a fixture site already installed in this registry.
    fn capture<R, S, X, A>(site: &LocalMountSite<R, S, X, A>, scopes: &ScopeRegistry) -> Self {
        let reservation = site.output_registration();
        let identity = reservation.as_ptr().cast::<()>() as usize;
        let frozen = reservation.upgrade().unwrap().freeze(scopes).unwrap();
        Self {
            reservation,
            identity,
            frozen,
        }
    }
}
/// Reuses the shared walk with local callback acquisition and local values.
///
/// # Errors
/// Returns an error if the source owner, roster entry or captured installation expired.
fn query<R: 'static, A, K: Contribution>(
    root: &R,
    scopes: &ScopeRegistry,
    mounts: &[Box<<Local as Carriers<R, A>>::Mount>],
    index: &Index<R, K>,
    subtree: &Subtree,
) -> Result<(K::Value, Stats), ()> {
    let _reservation = subtree.reservation.upgrade().ok_or(())?;
    walk::<R, A, K, Local, _>(
        root,
        scopes,
        mounts,
        subtree.identity,
        &subtree.frozen,
        |identity, scopes, stats| index.acquire(identity, scopes, stats),
    )
}

/// Allows same-key replacement of the selected parent.
struct State {
    rows: crate::KeyedState<u8, Row>,
}
/// Supplies the optional leaf selected by the real local route.
struct Row {
    value: Option<usize>,
}
/// Provides an inert local parent for the callback fixture.
struct Parent;
impl local::Feature for Parent {
    type State = Row;
    type Action = bool;
    fn update(&mut self, _: &mut Row, _: &bool, _: &mut local::FeatureContext<'_, Row, bool>) {}
}
/// Provides an installed local leaf without reducer-side work.
struct Leaf;
impl local::Feature for Leaf {
    type State = usize;
    type Action = u8;
    fn update(&mut self, _: &mut usize, _: &u8, _: &mut local::FeatureContext<'_, usize, u8>) {}
}
/// Moves a non-`Send` value without `Clone`, `Debug` or equality implementations.
struct Owned(Rc<usize>);
/// Exposes callback order through decimal accumulation of local owned values.
struct Digits;
impl Contribution for Digits {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Rc::new(0))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        ControlFlow::Continue(Owned(Rc::new(*accumulator.0 * 10 + *next.0)))
    }
}
/// Owns real local declarations and the private prototype's weak callback index.
struct Fixture {
    /// Holds the parent and selected leaf read by installed routes.
    state: State,
    /// Tracks installed generations and invalidates same-key predecessors.
    scopes: ScopeRegistry,
    /// Keeps the fixture actor lifetime open during route use.
    _lifecycle: Arc<Lifecycle>,
    /// Keeps weak declaration bindings valid without launching an actor.
    _ingress: Arc<CommandIngress<Command<State, char, Local>>>,
    /// Owns declarations and callback metadata in committed preorder.
    mounts: Vec<Box<<Local as Carriers<State, char>>::Mount>>,
    /// Retains installed feature nodes during structural reconciliation.
    nodes: Vec<Box<<Local as Carriers<State, char>>::Node>>,
    /// Resolves callback cells without extending their payload lifetimes.
    index: Index<State, Digits>,
    /// Names the selected parent used to capture a subtree.
    site: LocalMountSite<State, Row, bool, char>,
    /// Records actual callback order independently of the accumulated result.
    trace: Rc<RefCell<Vec<usize>>>,
    /// Observes whether predecessor callback captures have been destroyed.
    #[allow(
        dead_code,
        reason = "ordinary-test readers are absent in the allocation executable; retain fixture layout and capture lifetime"
    )]
    captures: Rc<RefCell<Vec<LocalWeak<()>>>>,
    /// Injects one callback panic before any contribution is recorded.
    #[allow(
        dead_code,
        reason = "ordinary-test readers are absent in the allocation executable; retain fixture layout and panic probe lifetime"
    )]
    panic_once: Rc<Cell<bool>>,
}
impl Fixture {
    /// Installs parent 1, leaf 2 and an explicit parent-times-ten transform.
    fn new() -> Self {
        let index = Index::new();
        let recipe_index = index.clone();
        let parent_site = Rc::new(OnceLock::<LocalMountSite<State, Row, bool, char>>::new());
        let recipe_site = std::rc::Rc::clone(&parent_site);
        let captures = Rc::new(RefCell::new(Vec::<LocalWeak<()>>::new()));
        let trace = Rc::new(RefCell::new(Vec::new()));
        let panic_once = Rc::new(Cell::new(false));
        let old_captures = std::rc::Rc::clone(&captures);
        let recipe_captures = std::rc::Rc::clone(&captures);
        let recipe_trace = std::rc::Rc::clone(&trace);
        let recipe_panic = std::rc::Rc::clone(&panic_once);
        let (owner, site) = attach_selected_key::<State, char, Parent, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            7,
            move |_| {
                assert!(
                    old_captures
                        .borrow()
                        .iter()
                        .all(|weak| weak.upgrade().is_none())
                );
                Parent
            },
            move |_, children| {
                let capture = Rc::new(());
                recipe_captures.borrow_mut().push(Rc::downgrade(&capture));
                let trace = std::rc::Rc::clone(&recipe_trace);
                let panic_once = std::rc::Rc::clone(&recipe_panic);
                recipe_index.register(
                    children,
                    recipe_site.get().unwrap(),
                    children.depth,
                    move |_| {
                        let _ = &capture;
                        assert!(!panic_once.replace(false), "callback unwind");
                        trace.borrow_mut().push(1);
                        Owned(Rc::new(1))
                    },
                    Some(Box::new(|value: Owned| Owned(Rc::new(*value.0 * 10)))),
                );
                let leaf = children.optional(LocalOptionalMount::new(
                    |row: &Row| row.value.as_ref(),
                    |row| row.value.as_mut(),
                    |row| {
                        row.value.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Leaf,
                ));
                let trace = std::rc::Rc::clone(&recipe_trace);
                recipe_index.register(
                    children,
                    &leaf,
                    children.depth + 1,
                    move |value| {
                        trace.borrow_mut().push(*value);
                        Owned(Rc::new(*value))
                    },
                    None,
                );
            },
        );
        parent_site.set(site.clone()).unwrap();
        let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "exercise the actual Arc ingress carrier with deliberately local callback payloads"
        )]
        let ingress = Arc::new(CommandIngress::new(sender, ControlWake::new()));
        let (lifecycle, scopes) = ScopeRegistry::for_test();
        owner.bind(&ingress, &lifecycle);
        let mut fixture = Self {
            state: State {
                rows: [(7, Row { value: Some(2) })].into_iter().collect(),
            },
            scopes,
            _lifecycle: lifecycle,
            _ingress: ingress,
            mounts: vec![owner],
            nodes: Vec::new(),
            index,
            site,
            trace,
            captures,
            panic_once,
        };
        fixture.reconcile();
        fixture
    }
    /// Reuses real reconciliation after replacing the selected parent state.
    fn reconcile(&mut self) {
        super::super::nested::reconcile_roster::<_, _, Local>(
            &mut self.mounts,
            true,
            &self.state,
            &mut self.scopes,
            &mut ScopeTransitionBatch::default(),
            &mut self.nodes,
            #[cfg(feature = "diagnostics")]
            None,
        );
    }
}
#[test]
fn local_fold_gate_reuses_walk_with_rc_values_callbacks_and_explicit_transform() {
    let fixture = Fixture::new();
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    let (value, stats) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(*value.0, 120);
    assert_eq!(*fixture.trace.borrow(), [1, 2]);
    assert_eq!(stats.callbacks, 2);
    assert_eq!(stats.max_frames, 1);
}
#[test]
fn local_fold_gate_restores_payload_after_callback_unwind() {
    let fixture = Fixture::new();
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    fixture.panic_once.set(true);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree,
        )
    }))
    .err()
    .expect("the selected contribution must panic");
    let message = panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
    assert_eq!(message, Some("callback unwind"));
    let (value, _) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(*value.0, 120);
}
#[test]
fn local_fold_gate_parent_replacement_drains_rc_captures_and_rejects_old_subtree() {
    let mut fixture = Fixture::new();
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    fixture.state.rows.remove(&7);
    fixture.state.rows.insert(7, Row { value: Some(8) });
    fixture.reconcile();
    assert!(fixture.captures.borrow()[0].upgrade().is_none());
    assert!(
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree
        )
        .is_err()
    );
    drop(fixture.mounts);
    assert!(
        fixture
            .captures
            .borrow()
            .iter()
            .all(|weak| weak.upgrade().is_none())
    );
    assert!(!fixture.index.0.borrow().is_empty());
}

/// Compares literal local callbacks with cold and warm shared-walk queries.
///
/// The caller owns the profiler. Callback tracing is preallocated before work.
/// Local value allocation and explicit transform frames remain part of each sample.
#[cfg(feature = "allocation-measurement")]
pub(in crate::store::mount) fn run_costs() {
    use crate::store::mount::allocation_tests::measure;
    let (fixture, construction) = measure(Fixture::new);
    fixture.trace.borrow_mut().reserve(2);
    let (subtree, capture) = measure(|| Subtree::capture(&fixture.site, &fixture.scopes));
    let identities: Vec<_> = fixture
        .mounts
        .iter()
        .filter_map(|owner| owner.registration_identity())
        .collect();
    assert_eq!(identities.len(), 2);
    let ((value, direct_stats), direct) = measure(|| {
        let mut stats = Stats::default();
        let parent = fixture
            .index
            .acquire(identities[0], &fixture.scopes, &mut stats)
            .unwrap();
        let mut accumulator = Some(Digits::identity());
        let value = parent.entry().contribution(&fixture.state).unwrap();
        stats.callbacks += 1;
        assert!(!super::super::nested::fold_gate::feed::<Digits>(
            &mut accumulator,
            value
        ));
        let child = fixture
            .index
            .acquire(identities[1], &fixture.scopes, &mut stats)
            .unwrap();
        let value = child.entry().contribution(&fixture.state).unwrap();
        stats.callbacks += 1;
        assert!(!super::super::nested::fold_gate::feed::<Digits>(
            &mut accumulator,
            value
        ));
        let transformed = parent.entry().transform(accumulator.take().unwrap());
        stats.transforms += 1;
        let mut outer = Some(Digits::identity());
        assert!(!super::super::nested::fold_gate::feed::<Digits>(
            &mut outer,
            transformed
        ));
        (outer.unwrap(), stats)
    });
    assert_eq!(*value.0, 120);
    assert_eq!(*fixture.trace.borrow(), [1, 2]);
    fixture.trace.borrow_mut().clear();
    let ((value, cold_stats), cold) = measure(|| {
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree,
        )
        .unwrap()
    });
    assert_eq!(*value.0, 120);
    fixture.trace.borrow_mut().clear();
    let ((value, warm_stats), warm) = measure(|| {
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree,
        )
        .unwrap()
    });
    assert_eq!(*value.0, 120);
    assert_eq!(warm_stats.max_frames, 1);
    eprintln!(
        "private fold Local blocks/bytes: fixture construction={construction:?}; captured subtree={capture:?}; literal same-route callbacks+transform={direct:?}; query cold={cold:?}; query warm={warm:?}"
    );
    eprintln!(
        "private fold Local counters: literal={direct_stats:?}; cold={cold_stats:?}; warm={warm_stats:?}"
    );
}
