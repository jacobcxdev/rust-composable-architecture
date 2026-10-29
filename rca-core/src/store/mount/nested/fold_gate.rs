//! Private K/R compiler gate; not a public Fold or ordered-read implementation.
//!
//! This test-only prototype reuses real declarations and frozen output routes.
//! Actor-serial queries lease callbacks outside locks. Its weak index is a fixture
//! mechanism, not a production indexing design or a concurrent query API.

use super::{
    Arc, Carriers, Cell, ChildMounts, Command, Feature, Lifecycle, MountDeclaration, MountIdentity,
    MountPosition, MountSite, OnceLock, OptionalMount, ScopeRegistry, ScopeToken,
    ScopeTransitionBatch, Shared, attach_selected_key, reconcile_roster,
};
use crate::effect::lineage::ControlWake;
use crate::store::ingress::CommandIngress;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Weak;

#[cfg(feature = "allocation-measurement")]
mod costs;
/// Runs Shared and available Local probes inside the caller's allocation profiler.
#[cfg(feature = "allocation-measurement")]
pub(in crate::store::mount) fn run_costs() {
    costs::run();
    #[cfg(feature = "local")]
    super::super::local::fold_gate::run_costs();
}

/// Moves contribution values through an ordered reduction without cloning requirements.
pub(in crate::store::mount) trait Contribution: 'static {
    /// Owns one intermediate or final value without additional trait bounds.
    type Value: 'static;
    /// Creates the empty result for one fold or explicit transform frame.
    fn identity() -> Self::Value;
    /// Combines the next value, or stops this accumulator with an owned result.
    fn reduce(accumulator: Self::Value, next: Self::Value)
    -> ControlFlow<Self::Value, Self::Value>;
}

/// Supplies leased callbacks against one current installed route.
pub(in crate::store::mount) trait PureEntry<R, K: Contribution> {
    /// Captures a current route and rejects missing or stale installations.
    fn freeze(&mut self, scopes: &ScopeRegistry) -> bool;
    /// Reads the selected state and returns an owned contribution, if present.
    fn contribution(&self, root: &R) -> Option<K::Value>;
    /// Indicates whether traversal needs a separate subtree accumulator.
    fn has_transform(&self) -> bool;
    /// Transforms a completed subtree value when `has_transform` is true.
    fn transform(&self, value: K::Value) -> K::Value;
    /// Releases the frozen route before the callback payload returns to its owner.
    fn clear_route(&mut self);
}

/// Keeps typed callbacks and weak registration metadata together for one declaration.
struct TypedEntry<R, A, S, X, K: Contribution> {
    /// Reserves declaration identity without retaining its callback payload.
    registration: Weak<dyn crate::store::output::HandlerRegistration<R, S, X, A>>,
    /// Exists only while a query leases this callback entry.
    route: Option<Box<dyn crate::store::output::HandlerRoute<R, A, S, X> + Send>>,
    /// Produces an owned value from the installed child state.
    #[expect(
        clippy::type_complexity,
        reason = "keep the Send contribution callback ownership explicit in the private gate"
    )]
    contribution: Box<dyn Fn(&S) -> K::Value + Send>,
    /// Adds an explicit subtree boundary only when present.
    #[expect(
        clippy::type_complexity,
        reason = "keep the optional owned Send value transform explicit in the private gate"
    )]
    transform: Option<Box<dyn Fn(K::Value) -> K::Value + Send>>,
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
        self.transform.as_ref().expect("explicit transform")(value)
    }
    fn clear_route(&mut self) {
        self.route = None;
    }
}

/// Separates payload ownership from the guard used to transfer it.
struct EntryCell<R, K: Contribution> {
    /// Is empty while one actor-serial callback lease owns the payload.
    payload: Mutex<Option<Box<dyn PureEntry<R, K> + Send>>>,
}
/// Attaches one callback cell to the source declaration's original lifetime.
struct EntryOwner<R, K: Contribution> {
    /// Owns the callback payload unless the current query temporarily leases it.
    cell: Arc<EntryCell<R, K>>,
    /// Resolves the source's installed scope without retaining that source.
    source: Weak<dyn crate::store::output::OutputRegistration>,
    /// Freezes the original parent scope rather than following a replacement.
    parent: Cell<Option<ScopeToken>>,
    /// Places this metadata owner immediately below its source in the roster.
    depth: usize,
}
impl<R, K: Contribution> EntryOwner<R, K> {
    /// Takes the payload under its lock and destroys captures after unlocking.
    fn drain(&self) {
        let payload = self.cell.payload.lock().take();
        drop(payload);
    }
}
impl<R, K: Contribution> Drop for EntryOwner<R, K> {
    fn drop(&mut self) {
        self.drain();
    }
}
impl<R, A, K: Contribution> MountDeclaration<R, A> for EntryOwner<R, K> {
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
    fn bind(
        &self,
        _: &Arc<crate::store::ingress::CommandIngress<Command<R, A>>>,
        _: &Arc<Lifecycle>,
    ) {
    }
    fn reconcile(
        &self,
        _: &R,
        scopes: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>,
        _: &mut super::Descendants<'_, R, A, Shared>,
        #[cfg(feature = "diagnostics")] _: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        // The target declaration precedes this owner in the one committed roster.
        // Freeze once, so replacement never retargets the callback owner's parent.
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

/// Retains weak callback cells indexed by reserved declaration addresses.
#[expect(
    clippy::type_complexity,
    reason = "keep the shared weak index ownership explicit in the private gate"
)]
struct Index<R, K: Contribution>(
    /// Holds address reservations and weak cells, never strong callback owners.
    Arc<Mutex<Vec<(usize, Weak<EntryCell<R, K>>)>>>,
);
impl<R, K: Contribution> Clone for Index<R, K> {
    fn clone(&self) -> Self {
        Self(std::sync::Arc::clone(&self.0))
    }
}
impl<R: 'static, K: Contribution> Index<R, K> {
    /// Creates an empty index shared by the fixture and its recipes.
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    /// Registers one owned callback below its source declaration.
    #[expect(
        clippy::type_complexity,
        reason = "keep the shared registration callback and transform ownership explicit"
    )]
    fn register<A: Send + 'static, S: Send + 'static, X: Send + 'static, P>(
        &self,
        children: &mut ChildMounts<'_, R, A, P>,
        site: &MountSite<R, S, X, A>,
        target_depth: usize,
        contribution: impl Fn(&S) -> K::Value + Send + 'static,
        transform: Option<Box<dyn Fn(K::Value) -> K::Value + Send>>,
    ) where
        R: Send,
        K::Value: Send,
    {
        let registration = site.handler_registration();
        let identity = registration.as_ptr().cast::<()>() as usize;
        let cell = Arc::new(EntryCell {
            payload: Mutex::new(Some(Box::new(TypedEntry::<_, _, _, _, K> {
                registration,
                route: None,
                contribution: Box::new(contribution),
                transform,
            }))),
        });
        self.0.lock().push((identity, Arc::downgrade(&cell)));
        children.discovered.push(Box::new(EntryOwner {
            cell,
            source: site.output_registration(),
            parent: Cell::new(None),
            depth: target_depth + 1,
        }));
    }
    /// Leases a live entry outside both locks, then freezes its current route.
    fn acquire(
        &self,
        identity: usize,
        scopes: &ScopeRegistry,
        stats: &mut Stats,
    ) -> Option<EntryLease<R, K>> {
        let cell = {
            let mut entries = self.0.lock();
            entries.retain(|(_, cell)| cell.strong_count() != 0);
            entries.iter().find_map(|(current, cell)| {
                stats.comparisons += 1;
                // Upgrade before comparing an address: the live entry retains
                // its declaration Weak, preventing ABA even after Feature drop.
                let cell = cell.upgrade()?;
                (*current == identity).then_some(cell)
            })
        }?;
        let payload = cell.payload.lock().take()?;
        let mut lease = EntryLease {
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
/// Returns callback ownership after normal return or unwind, outside the payload lock.
struct EntryLease<R, K: Contribution> {
    /// Keeps the restore slot alive for this actor-serial query.
    cell: Arc<EntryCell<R, K>>,
    /// Owns callbacks while the cell is empty.
    payload: Option<Box<dyn PureEntry<R, K> + Send>>,
}
/// Lets the shared walk borrow callbacks without knowing their ownership carrier.
pub(in crate::store::mount) trait CallbackLease<R, K: Contribution> {
    /// Borrows the payload that this lease exclusively owns.
    fn entry(&self) -> &dyn PureEntry<R, K>;
}
impl<R, K: Contribution> CallbackLease<R, K> for EntryLease<R, K> {
    fn entry(&self) -> &dyn PureEntry<R, K> {
        self.payload.as_deref().unwrap()
    }
}
impl<R, K: Contribution> Drop for EntryLease<R, K> {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.as_mut() {
            payload.clear_route();
        }
        {
            let mut destination = self.cell.payload.lock();
            std::mem::swap(&mut *destination, &mut self.payload);
        };
        drop(self.payload.take());
    }
}

/// Counts fixture traversal work without claiming machine instruction counts.
#[derive(Debug, Default)]
pub(in crate::store::mount) struct Stats {
    /// Roster positions inspected while locating the captured source.
    search_visits: usize,
    /// Positions inspected from the source, including a stopping sibling boundary.
    pub(in crate::store::mount) visits: usize,
    /// Weak-index entries examined during callback acquisition.
    pub(in crate::store::mount) comparisons: usize,
    /// Present state contributions actually invoked.
    pub(in crate::store::mount) callbacks: usize,
    /// Explicit subtree transforms actually invoked.
    pub(in crate::store::mount) transforms: usize,
    /// Callback routes successfully frozen for this query.
    pub(in crate::store::mount) frozen_routes: usize,
    /// Greatest number of simultaneously active explicit transform frames.
    pub(in crate::store::mount) max_frames: usize,
    /// Greatest scratch-vector capacity observed during this query.
    pub(in crate::store::mount) frame_capacity: usize,
}

/// Captures a declaration address and installation without owning callback payloads.
struct Subtree {
    /// Prevents address reuse while the captured identity remains observable.
    reservation: Weak<dyn crate::store::output::OutputRegistration>,
    /// Names the reserved declaration address in the committed roster.
    identity: usize,
    /// Rejects a later same-key installation or a foreign registry.
    frozen: crate::store::output::FrozenRegistration,
}
impl Subtree {
    /// Captures a fixture site that is already installed in this registry.
    fn capture<R, S, X, A>(site: &MountSite<R, S, X, A>, scopes: &ScopeRegistry) -> Self {
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

/// Holds an explicit transform's accumulator and callback lease until subtree exit.
struct Frame<K: Contribution, L> {
    /// Ends this frame when traversal reaches a sibling or ancestor depth.
    depth: usize,
    /// Moves through reduction without requiring the value to implement `Clone`.
    accumulator: Option<K::Value>,
    /// Suppresses later contributions inside this frame after an early stop.
    broken: bool,
    /// Retains the transform callback until the subtree result is complete.
    entry: L,
}
/// Moves the accumulator through one reduction and reports an early stop.
pub(in crate::store::mount) fn feed<K: Contribution>(
    accumulator: &mut Option<K::Value>,
    next: K::Value,
) -> bool {
    let (value, broken) = match K::reduce(accumulator.take().expect("one owned accumulator"), next)
    {
        ControlFlow::Continue(value) => (value, false),
        ControlFlow::Break(value) => (value, true),
    };
    *accumulator = Some(value);
    broken
}
/// Transforms one subtree, releases its lease, then feeds the enclosing accumulator.
fn finish_frame<R, K: Contribution, L: CallbackLease<R, K>>(
    frames: &mut Vec<Frame<K, L>>,
    accumulator: &mut Option<K::Value>,
    stats: &mut Stats,
) -> bool {
    let mut frame = frames.pop().unwrap();
    let value = frame
        .entry
        .entry()
        .transform(frame.accumulator.take().unwrap());
    stats.transforms += 1;
    // Retire this route/payload lease before reducing into the enclosing fold.
    drop(frame);
    if let Some(parent) = frames.last_mut() {
        parent.broken = feed::<K>(&mut parent.accumulator, value);
        false
    } else {
        feed::<K>(accumulator, value)
    }
}
/// Reads one captured subtree through the Shared callback index.
///
/// # Errors
/// Returns an error if the source owner, roster entry or captured installation expired.
fn query<R: 'static, A, K: Contribution>(
    root: &R,
    scopes: &ScopeRegistry,
    mounts: &[Box<<Shared as Carriers<R, A>>::Mount>],
    index: &Index<R, K>,
    subtree: &Subtree,
) -> Result<(K::Value, Stats), ()> {
    let _reservation = subtree.reservation.upgrade().ok_or(())?;
    walk::<R, A, K, Shared, _>(
        root,
        scopes,
        mounts,
        subtree.identity,
        &subtree.frozen,
        |identity, scopes, stats| index.acquire(identity, scopes, stats),
    )
}
/// Walks committed preorder with frames only for explicit subtree transforms.
///
/// Ordinary contributions share one accumulator. Early stops skip later callbacks
/// in that accumulator, while enclosing transform boundaries still complete.
///
/// # Errors
/// Returns an error for stale capture, missing declaration or a different installed scope.
pub(in crate::store::mount) fn walk<
    R: 'static,
    A,
    K: Contribution,
    C: Carriers<R, A>,
    L: CallbackLease<R, K>,
>(
    root: &R,
    scopes: &ScopeRegistry,
    mounts: &[Box<C::Mount>],
    identity: usize,
    frozen: &crate::store::output::FrozenRegistration,
    mut acquire: impl FnMut(usize, &ScopeRegistry, &mut Stats) -> Option<L>,
) -> Result<(K::Value, Stats), ()> {
    if !frozen.is_current(scopes) {
        return Err(());
    }
    let mut stats = Stats::default();
    let start = mounts
        .iter()
        .position(|owner| {
            stats.search_visits += 1;
            owner.registration_identity() == Some(identity)
        })
        .ok_or(())?;
    if mounts[start].installed_scope() != Some(frozen.scope) {
        return Err(());
    }
    let base_depth = mounts[start].depth();
    let mut accumulator = Some(K::identity());
    let mut frames: Vec<Frame<K, L>> = Vec::new();
    let mut broken = false;
    for (offset, owner) in mounts[start..].iter().enumerate() {
        let depth = owner.depth();
        stats.visits += 1;
        if offset != 0 && depth <= base_depth {
            break;
        }
        while frames.last().is_some_and(|frame| depth <= frame.depth) {
            broken |= finish_frame::<R, K, L>(&mut frames, &mut accumulator, &mut stats);
        }
        if broken {
            break;
        }
        if frames.last().is_some_and(|frame| frame.broken) {
            continue;
        }
        let Some(identity) = owner.registration_identity() else {
            continue;
        };
        if !owner
            .installed_scope()
            .is_some_and(|scope| scopes.is_current(scope))
        {
            continue;
        }
        let Some(entry) = acquire(identity, scopes, &mut stats) else {
            continue;
        };
        let transformed = entry.entry().has_transform();
        let value = if transformed {
            frames.push(Frame {
                depth,
                accumulator: Some(K::identity()),
                broken: false,
                entry,
            });
            stats.max_frames = stats.max_frames.max(frames.len());
            stats.frame_capacity = stats.frame_capacity.max(frames.capacity());
            frames.last().unwrap().entry.entry().contribution(root)
        } else {
            entry.entry().contribution(root)
        };
        if let Some(value) = value {
            stats.callbacks += 1;
            if let Some(frame) = frames.last_mut() {
                frame.broken = feed::<K>(&mut frame.accumulator, value);
            } else {
                broken = feed::<K>(&mut accumulator, value);
            }
        }
    }
    while !frames.is_empty() {
        broken |= finish_frame::<R, K, L>(&mut frames, &mut accumulator, &mut stats);
    }
    let _ = broken;
    Ok((accumulator.unwrap(), stats))
}

/// Supplies two independently optional leaves with distinct literal values.
struct Row {
    a: Option<usize>,
    b: Option<usize>,
}
/// Allows same-key replacement to exercise captured subtree invalidation.
struct State {
    rows: crate::KeyedState<u8, Row>,
}
/// Provides a stable parent feature without reducer-side work.
struct Parent;
impl Feature for Parent {
    type State = Row;
    type Action = bool;
    fn reduce(&mut self, _: &mut Row, _: &bool, _: &mut crate::FeatureContext<'_, Row, bool>) {}
}
/// Owns the real mount roster and the prototype's callback index.
struct Fixture<K: Contribution> {
    /// Holds parent and leaf values read by real installed routes.
    state: State,
    /// Tracks installation generations checked by every query.
    scopes: ScopeRegistry,
    /// Keeps the actor lifetime live while fixture routes are exercised.
    _lifecycle: Arc<Lifecycle>,
    /// Keeps weak declaration bindings valid without launching an actor.
    _ingress: Arc<CommandIngress<Command<State, char>>>,
    /// Owns declarations in committed preorder, including callback metadata owners.
    mounts: Vec<Box<<Shared as Carriers<State, char>>::Mount>>,
    /// Retains installed feature nodes during reconciliation.
    nodes: Vec<Box<<Shared as Carriers<State, char>>::Node>>,
    /// Resolves weak callback entries without retaining them after owner destruction.
    index: Index<State, K>,
    /// Captures the selected parent whose descendants contribute to the query.
    site: MountSite<State, Row, bool, char>,
}
impl<K: Contribution> Fixture<K>
where
    K::Value: Send,
{
    /// Installs parent 1 and leaves 2/3 with optional parent and first-leaf transforms.
    #[expect(
        clippy::too_many_lines,
        reason = "keep the parent and descendant callback fixture construction in declaration order"
    )]
    #[expect(
        clippy::type_complexity,
        reason = "keep the paired parent and descendant value transforms explicit in this fixture"
    )]
    fn new(
        transforms: (
            Option<fn(K::Value) -> K::Value>,
            Option<fn(K::Value) -> K::Value>,
        ),
        trace: Arc<Mutex<Vec<usize>>>,
        captures: Arc<Mutex<Vec<Weak<()>>>>,
        make_value: fn(usize) -> K::Value,
    ) -> Self {
        let index = Index::<State, K>::new();
        let recipe_index = index.clone();
        let parent_site = Arc::new(OnceLock::<MountSite<State, Row, bool, char>>::new());
        let recipe_site = std::sync::Arc::clone(&parent_site);
        let old_captures = std::sync::Arc::clone(&captures);
        let (owner, site) = attach_selected_key::<State, char, Parent, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            7,
            move |_| {
                assert!(
                    old_captures
                        .lock()
                        .iter()
                        .all(|capture| capture.upgrade().is_none()),
                    "old callbacks die before successor construction"
                );
                Parent
            },
            move |_, children| {
                let capture = Arc::new(());
                captures.lock().push(Arc::downgrade(&capture));
                let parent_trace = std::sync::Arc::clone(&trace);
                let send_not_sync = Cell::new(0);
                recipe_index.register(
                    children,
                    recipe_site.get().unwrap(),
                    children.depth,
                    move |_| {
                        let _ = &capture;
                        send_not_sync.set(send_not_sync.get() + 1);
                        parent_trace.lock().push(1);
                        make_value(1)
                    },
                    transforms.0.map(|transform| {
                        Box::new(transform) as Box<dyn Fn(K::Value) -> K::Value + Send>
                    }),
                );
                for (which, digit) in [(false, 2), (true, 3)] {
                    let read: for<'a> fn(&'a Row) -> Option<&'a usize> = if which {
                        |row| row.b.as_ref()
                    } else {
                        |row| row.a.as_ref()
                    };
                    let write: for<'a> fn(&'a mut Row) -> Option<&'a mut usize> = if which {
                        |row| row.b.as_mut()
                    } else {
                        |row| row.a.as_mut()
                    };
                    let leaf = children.optional(OptionalMount::new(
                        read,
                        write,
                        move |row: &Row| {
                            (if which { row.b } else { row.a }).map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        |_| Leaf,
                    ));
                    let trace = std::sync::Arc::clone(&trace);
                    recipe_index.register(
                        children,
                        &leaf,
                        children.depth + 1,
                        move |value| {
                            trace.lock().push(digit);
                            make_value(*value)
                        },
                        if which {
                            None
                        } else {
                            transforms.1.map(|transform| {
                                Box::new(transform) as Box<dyn Fn(K::Value) -> K::Value + Send>
                            })
                        },
                    );
                }
            },
        );
        parent_site.set(site.clone()).unwrap();
        let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        let ingress = Arc::new(CommandIngress::new(sender, ControlWake::new()));
        let (lifecycle, scopes) = ScopeRegistry::for_test();
        owner.bind(&ingress, &lifecycle);
        let mut fixture = Self {
            state: State {
                rows: [(
                    7,
                    Row {
                        a: Some(2),
                        b: Some(3),
                    },
                )]
                .into_iter()
                .collect(),
            },
            scopes,
            _lifecycle: lifecycle,
            _ingress: ingress,
            mounts: vec![owner],
            nodes: Vec::new(),
            index,
            site,
        };
        fixture.reconcile();
        fixture
    }
    /// Reuses real structural reconciliation after fixture state mutations.
    fn reconcile(&mut self) {
        reconcile_roster::<_, _, Shared>(
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

/// Makes callback order observable through non-associative decimal accumulation.
struct Digits;
impl Contribution for Digits {
    type Value = usize;
    fn identity() -> usize {
        0
    }
    fn reduce(accumulator: usize, next: usize) -> ControlFlow<usize, usize> {
        ControlFlow::Continue(accumulator * 10 + next)
    }
}
/// Stops after the first present contribution.
struct First;
impl Contribution for First {
    type Value = usize;
    fn identity() -> usize {
        0
    }
    fn reduce(_: usize, next: usize) -> ControlFlow<usize, usize> {
        ControlFlow::Break(next)
    }
}
/// Makes explicit subtree transform boundaries visible in literal sums.
struct Sum;
impl Contribution for Sum {
    type Value = usize;
    fn identity() -> usize {
        0
    }
    fn reduce(accumulator: usize, next: usize) -> ControlFlow<usize, usize> {
        ControlFlow::Continue(accumulator + next)
    }
}

#[test]
fn fold_gate_preserves_literal_non_associative_preorder_without_ordinary_frames() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let fixture = Fixture::<Digits>::new(
        (None, None),
        std::sync::Arc::clone(&trace),
        Arc::new(Mutex::new(Vec::new())),
        |value| value,
    );
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    let (value, stats) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(value, 123);
    assert_eq!(*trace.lock(), [1, 2, 3]);
    assert_eq!(stats.max_frames, 0);
    assert_eq!(stats.callbacks, 3);
}

#[test]
fn fold_gate_break_skips_later_callbacks_and_replacement_drops_old_captures() {
    let trace = Arc::new(Mutex::new(Vec::new()));
    let captures = Arc::new(Mutex::new(Vec::new()));
    let mut fixture = Fixture::<First>::new(
        (None, Some(|_| panic!("outer Break skips later transform"))),
        std::sync::Arc::clone(&trace),
        std::sync::Arc::clone(&captures),
        |value| value,
    );
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    let (value, stats) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(value, 1);
    assert_eq!(*trace.lock(), [1]);
    assert_eq!(stats.callbacks, 1);
    fixture.state.rows.remove(&7);
    fixture.state.rows.insert(
        7,
        Row {
            a: Some(8),
            b: Some(9),
        },
    );
    fixture.reconcile();
    assert!(captures.lock()[0].upgrade().is_none());
    assert!(
        query(
            &fixture.state,
            &fixture.scopes,
            &fixture.mounts,
            &fixture.index,
            &subtree
        )
        .is_err(),
        "captured subtree cannot silently follow same-key successor"
    );
    drop(fixture.mounts);
    assert!(
        captures
            .lock()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
    assert!(
        !fixture.index.0.lock().is_empty(),
        "retained weak index is not capture ownership"
    );
}

#[test]
fn fold_gate_transforms_only_explicit_subtrees_and_preserves_preceding_accumulator() {
    let mut fixture = Fixture::<Sum>::new(
        (None, Some(|value| value * 10)),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
        |value| value,
    );
    fixture.state.rows.get_mut(&7).unwrap().b = None;
    fixture.reconcile();
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    let (value, stats) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(value, 21, "1 + transformed(2 * 10), never (1 + 2) * 10");
    assert_eq!(stats.max_frames, 1);
    let fixture = Fixture::<Sum>::new(
        (Some(|value| value * 10), Some(|value| value * 10)),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
        |value| value,
    );
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    let (value, stats) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(value, 240, "(1 + transformed(2 * 10) + 3) * 10");
    assert_eq!(stats.max_frames, 2);
}

ordinary_test_items! {
/// Moves heap-owned values without `Clone`, `Debug` or equality implementations.
struct Owned(Box<usize>);
/// Applies ordered decimal accumulation to the move-only fixture value.
struct OwnedDigits;
impl Contribution for OwnedDigits {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Box::new(0))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        ControlFlow::Continue(Owned(Box::new(*accumulator.0 * 10 + *next.0)))
    }
}
}
#[test]
fn fold_gate_moves_owned_values_without_clone_debug_or_equality_bounds() {
    let fixture = Fixture::<OwnedDigits>::new(
        (None, None),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
        |value| Owned(Box::new(value)),
    );
    let subtree = Subtree::capture(&fixture.site, &fixture.scopes);
    let (value, _) = query(
        &fixture.state,
        &fixture.scopes,
        &fixture.mounts,
        &fixture.index,
        &subtree,
    )
    .unwrap();
    assert_eq!(*value.0, 123);
}

/// Supplies installed leaf routes without additional reducer behaviour.
struct Leaf;
impl Feature for Leaf {
    type State = usize;
    type Action = u8;
    fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut crate::FeatureContext<'_, usize, u8>) {}
}

#[test]
fn fold_gate_reads_installed_route_and_same_declaration_identity() {
    let (owner, site) =
        crate::store::mount::attach::<Option<usize>, bool, _, _>(OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Leaf,
        ));
    let registration = site.handler_registration();
    // This weak reservation outlives every comparison, preventing address reuse.
    let identity = registration.as_ptr().cast::<()>() as usize;
    assert_eq!(owner.registration_identity(), Some(identity));
    let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<Option<usize>, bool>>::new(
        sender,
        ControlWake::new(),
    ));
    let (lifecycle, mut scopes) = ScopeRegistry::for_test();
    owner.bind(&ingress, &lifecycle);
    let mut mounts = vec![owner];
    let mut nodes = Vec::new();
    let state = Some(7);
    reconcile_roster::<_, _, Shared>(
        &mut mounts,
        false,
        &state,
        &mut scopes,
        &mut ScopeTransitionBatch::default(),
        &mut nodes,
        #[cfg(feature = "diagnostics")]
        None,
    );
    let route = registration
        .upgrade()
        .unwrap()
        .freeze_handler_route(&scopes)
        .unwrap();
    assert!(route.registration().is_current(&scopes));
    assert_eq!(route.read(&state), Some(&7));
}
