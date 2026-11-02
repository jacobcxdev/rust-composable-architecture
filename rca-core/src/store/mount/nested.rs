//! Actor-owned finite descendant discovery over the existing declaration roster.
//!
//! The roster stores declarations in depth-first order. Each active iterator
//! owns its unvisited prefix, so retirement can consume an old subtree before
//! any successor recipe runs. New descendants enter explicit iterator frames
//! instead of recursive calls. The flat path keeps its existing allocation.

#[cfg(all(test, feature = "test-util"))]
pub(super) mod fold_gate;

#[cfg(test)]
use super::super::mounted::path::{ComposedPath, KeyedPath};
use super::{
    Arc, Batch, Binding, Carriers, Command, Lifecycle, MountDeclaration, MountPosition, OnceLock,
    ScopeRegistry, ScopeToken, ScopeTransitionBatch, reaction, with_payload,
};
#[cfg(test)]
use super::{
    BorrowPath, Descriptor, Feature, Hash, MountIdentity, MountSite, OptionalMount, Shared,
    attach_descriptor,
};
use std::cell::{Cell, RefCell};

/// Borrows the unvisited descendant prefix of one depth-first iterator frame.
pub(crate) struct Descendants<'a, R, A, C: Carriers<R, A>> {
    /// Remaining owned declarations; absent on the flat, recipe-free path.
    remaining: Option<&'a mut std::vec::IntoIter<Box<C::Mount>>>,
    /// Parent depth that bounds the contiguous descendant prefix.
    depth: usize,
    /// Whether collection reconciliation changed this parent's child ordering.
    pub(super) reordered: bool,
}

impl<R, A, C: Carriers<R, A>> Descendants<'_, R, A, C> {
    /// Represents a declaration whose construction cannot discover descendants.
    pub(super) fn empty() -> Self {
        Self {
            remaining: None,
            depth: 0,
            reordered: false,
        }
    }

    /// Borrows descendants up to, but not including, the next sibling or ancestor.
    pub(super) fn inspect(&self) -> &[Box<C::Mount>] {
        let Some(remaining) = &self.remaining else {
            return &[];
        };
        let slice = remaining.as_slice();
        let end = slice
            .iter()
            .take_while(|child| child.depth() > self.depth)
            .count();
        #[cfg(all(test, feature = "allocation-measurement"))]
        super::collection_costs::record_inspected(end);
        &slice[..end]
    }

    /// Moves the descendant prefix into collection reconciliation's replacement roster.
    ///
    /// Collection declarations always run inside a real iterator frame. The
    /// flat path cannot call this method because it contains no collections.
    pub(super) fn take(&mut self) -> Vec<Box<C::Mount>> {
        let count = self.inspect().len();
        #[cfg(all(test, feature = "allocation-measurement"))]
        super::collection_costs::record_moved(count);
        self.remaining
            .as_mut()
            .expect("collection reconciliation owns a DFS frame")
            .by_ref()
            .take(count)
            .collect()
    }

    /// Drops the already-invalidated subtree in reverse depth-first order.
    ///
    /// Each child drops before its ancestor. Siblings also drop in reverse
    /// roster order. If a destructor panics, the owning iterator still owns
    /// every unvisited declaration for unwinding cleanup.
    pub(super) fn retire(&mut self) {
        // The parent scope and its entire subtree were invalidated before this
        // callback. Consume only the old DFS subtree, child-first, before its
        // parent. Reversing its owned prefix needs no allocation and leaves
        // siblings in place, without a later pass over drained declarations.
        let count = self.inspect().len();
        if let Some(remaining) = &mut self.remaining {
            remaining.as_mut_slice()[..count].reverse();
            for child in remaining.by_ref().take(count) {
                drop(child);
            }
        }
    }
}

/// Reconciles the finite roster, visiting newly declared descendants before later siblings.
///
/// `has_recipes` comes from construction, not from the current child count. The
/// flat path therefore cannot discover work. The nested path preserves unvisited
/// ownership after root closure but enters no more user callbacks. A panic ends
/// the actor; this function does not restore a partially reconciled roster.
pub(in crate::store) fn reconcile_roster<R, A, C: Carriers<R, A>>(
    mounts: &mut Vec<Box<C::Mount>>,
    has_recipes: bool,
    state: &R,
    scopes: &mut ScopeRegistry,
    transitions: &mut ScopeTransitionBatch,
    mounted: &mut Vec<Box<C::Node>>,
    #[cfg(feature = "diagnostics")] mut lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
) {
    if !has_recipes {
        // Construction proves these declarations cannot discover descendants.
        // Keep the ordinary flat roster and its backing allocation unchanged.
        for owner in mounts.iter() {
            if !scopes.installation_is_live() {
                break;
            }
            owner.reconcile(
                state,
                scopes,
                transitions,
                mounted,
                scopes.root(),
                &mut Vec::new(),
                &mut Descendants::empty(),
                #[cfg(feature = "diagnostics")]
                lifecycle
                    .as_mut()
                    .map(crate::diagnostics::LifecycleCall::reborrow),
            );
        }
        return;
    }
    let old = std::mem::take(mounts);
    let has_collections = old.iter().any(|owner| owner.can_reorder());
    let mut reordered_parents = std::collections::HashSet::new();
    let mut next: Vec<Box<C::Mount>> = Vec::with_capacity(old.len());
    let mut frames = vec![(old.into_iter(), None::<usize>)];
    while let Some((frame, _)) = frames.last_mut() {
        let Some(owner) = frame.next() else {
            let (_, completed) = frames.pop().expect("current frame");
            if scopes.installation_is_live()
                && let Some(index) = completed
            {
                next[index].finish_reconcile();
            }
            continue;
        };
        let position = owner.position();
        if position
            .parent
            .is_some_and(|parent| !scopes.is_current(parent))
        {
            continue;
        }
        if !scopes.installation_is_live() {
            // Preserve remaining ownership for normal actor disposal, without
            // entering any more selectors, constructors or recipes.
            next.push(owner);
            for (frame, _) in frames.iter_mut().rev() {
                next.extend(frame.by_ref());
            }
            break;
        }
        let depth = position.depth;
        let mut descendants = Descendants {
            remaining: Some(&mut frames.last_mut().expect("current frame").0),
            depth,
            reordered: false,
        };
        let mut discovered = Vec::new();
        let previous = owner.installed_scope();
        let parent = position.parent.unwrap_or_else(|| scopes.root());
        owner.reconcile(
            state,
            scopes,
            transitions,
            mounted,
            parent,
            &mut discovered,
            &mut descendants,
            #[cfg(feature = "diagnostics")]
            lifecycle
                .as_mut()
                .map(crate::diagnostics::LifecycleCall::reborrow),
        );
        if descendants.reordered || (has_collections && previous != owner.installed_scope()) {
            reordered_parents.insert(parent);
        }
        if discovered.is_empty() && scopes.installation_is_live() {
            owner.finish_reconcile();
        }
        let index = next.len();
        next.push(owner);
        if !discovered.is_empty() {
            frames.push((discovered.into_iter(), Some(index)));
        }
    }
    if scopes.installation_is_live() && !reordered_parents.is_empty() {
        relink_reordered_children::<R, A, C>(&next, reordered_parents, scopes);
    }
    *mounts = next;
}

/// Restores registry sibling order only for parents whose roster changed.
fn relink_reordered_children<R, A, C: Carriers<R, A>>(
    mounts: &[Box<C::Mount>],
    parents: std::collections::HashSet<ScopeToken>,
    scopes: &mut ScopeRegistry,
) {
    let mut desired: std::collections::HashMap<_, Vec<_>> = parents
        .into_iter()
        .map(|parent| (parent, Vec::new()))
        .collect();
    for owner in mounts {
        if let Some(token) = owner.installed_scope()
            && let Some(parent) = scopes.parent(token)
            && let Some(siblings) = desired.get_mut(&parent)
        {
            siblings.push(token);
        }
    }
    for (parent, siblings) in desired {
        scopes.relink_native_children(parent, &siblings);
    }
}

/// Adds the enclosing lifetime and depth to an existing declaration owner.
pub(super) struct NestedOwner<I> {
    /// Concrete declaration; callbacks and retirement remain owned by it.
    pub(super) inner: I,
    /// Parent incarnation, checked before this declaration is reconciled.
    pub(super) parent: ScopeToken,
    /// Depth in the flat roster, including non-Feature declaration entries.
    pub(super) depth: usize,
}

impl<R, A, C: Carriers<R, A>, I: MountDeclaration<R, A, C>> MountDeclaration<R, A, C>
    for NestedOwner<I>
{
    fn can_reorder(&self) -> bool {
        self.inner.can_reorder()
    }

    fn depth(&self) -> usize {
        self.depth
    }

    fn position(&self) -> MountPosition<'_> {
        MountPosition {
            depth: self.depth,
            parent: Some(self.parent),
            ..self.inner.position()
        }
    }

    fn installed_scope(&self) -> Option<ScopeToken> {
        self.inner.installed_scope()
    }

    fn finish_reconcile(&self) {
        self.inner.finish_reconcile();
    }

    fn registration_identity(&self) -> Option<usize> {
        self.inner.registration_identity()
    }

    fn has_reactions(&self) -> bool {
        self.inner.has_reactions()
    }

    fn capture_reactions(
        &self,
        state: &R,
        scopes: &ScopeRegistry,
        frame: &mut reaction::Frame<R, Batch<R, A, C>>,
    ) {
        self.inner.capture_reactions(state, scopes, frame);
    }

    fn reaction_entry(&self) -> Option<(ScopeToken, &dyn super::ReactionRunner<R, A, C>)> {
        self.inner.reaction_entry()
    }

    fn bind(
        &self,
        ingress: &Arc<super::super::ingress::CommandIngress<Command<R, A, C>>>,
        lifecycle: &Arc<Lifecycle>,
    ) {
        self.inner.bind(ingress, lifecycle);
    }

    fn reconcile(
        &self,
        state: &R,
        scopes: &mut ScopeRegistry,
        transitions: &mut ScopeTransitionBatch,
        mounted: &mut Vec<Box<C::Node>>,
        parent: ScopeToken,
        discovered: &mut Vec<Box<C::Mount>>,
        retire_descendants: &mut Descendants<'_, R, A, C>,
        #[cfg(feature = "diagnostics")] lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        self.inner.reconcile(
            state,
            scopes,
            transitions,
            mounted,
            parent,
            discovered,
            retire_descendants,
            #[cfg(feature = "diagnostics")]
            lifecycle,
        );
    }
}

/// Runs an owned child recipe once for each selectable parent incarnation.
pub(super) struct RecipeOwner<R, A, C: Carriers<R, A>, I, G> {
    /// Parent declaration reconciled before its recipe can run.
    inner: I,
    /// Callback returns whether it entered the user recipe. A panic consumes it.
    recipe: RefCell<Option<G>>,
    /// Last token whose recipe ran; missing selection leaves discovery pending.
    observed: Cell<Option<ScopeToken>>,
    /// Weak actor binding passed to newly discovered declarations.
    binding: OnceLock<Binding<R, A, C>>,
}

impl<R, A, C: Carriers<R, A>, I, G> RecipeOwner<R, A, C, I, G> {
    /// Retains a recipe without selecting State or declaring descendants yet.
    pub(super) fn new(inner: I, recipe: G) -> Self {
        Self {
            inner,
            recipe: RefCell::new(Some(recipe)),
            observed: Cell::new(None),
            binding: OnceLock::new(),
        }
    }
}

impl<R, A, C: Carriers<R, A>, I: MountDeclaration<R, A, C>, G> MountDeclaration<R, A, C>
    for RecipeOwner<R, A, C, I, G>
where
    G: FnMut(&R, ScopeToken, usize, &Binding<R, A, C>, &mut Vec<Box<C::Mount>>) -> bool,
{
    fn can_discover(&self) -> bool {
        true
    }

    fn can_reorder(&self) -> bool {
        self.inner.can_reorder()
    }

    fn depth(&self) -> usize {
        self.inner.depth()
    }

    fn position(&self) -> MountPosition<'_> {
        self.inner.position()
    }

    fn installed_scope(&self) -> Option<ScopeToken> {
        self.inner.installed_scope()
    }

    fn finish_reconcile(&self) {
        self.inner.finish_reconcile();
    }

    fn registration_identity(&self) -> Option<usize> {
        self.inner.registration_identity()
    }

    fn has_reactions(&self) -> bool {
        self.inner.has_reactions()
    }

    fn capture_reactions(
        &self,
        state: &R,
        scopes: &ScopeRegistry,
        frame: &mut reaction::Frame<R, Batch<R, A, C>>,
    ) {
        self.inner.capture_reactions(state, scopes, frame);
    }

    fn reaction_entry(&self) -> Option<(ScopeToken, &dyn super::ReactionRunner<R, A, C>)> {
        self.inner.reaction_entry()
    }

    fn bind(
        &self,
        ingress: &Arc<super::super::ingress::CommandIngress<Command<R, A, C>>>,
        lifecycle: &Arc<Lifecycle>,
    ) {
        self.inner.bind(ingress, lifecycle);
        let result = self.binding.set(Binding {
            ingress: Arc::downgrade(ingress),
            lifecycle: Arc::downgrade(lifecycle),
        });
        assert!(result.is_ok(), "consuming recipe binds one actor");
    }

    fn reconcile(
        &self,
        state: &R,
        scopes: &mut ScopeRegistry,
        transitions: &mut ScopeTransitionBatch,
        mounted: &mut Vec<Box<C::Node>>,
        parent: ScopeToken,
        discovered: &mut Vec<Box<C::Mount>>,
        retire_descendants: &mut Descendants<'_, R, A, C>,
        #[cfg(feature = "diagnostics")] lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        self.inner.reconcile(
            state,
            scopes,
            transitions,
            mounted,
            parent,
            discovered,
            retire_descendants,
            #[cfg(feature = "diagnostics")]
            lifecycle,
        );
        let installed = self.inner.installed_scope();
        let Some(token) = installed else {
            self.observed.set(None);
            return;
        };
        if installed == self.observed.get() || !scopes.installation_is_live() {
            return;
        }
        let binding = self
            .binding
            .get()
            .expect("actor binds recipe before reconciliation");
        // Own replacement retains this reusable recipe; only retired
        // descendant recipe owners lose their callbacks at the barrier.
        let entered = with_payload(&self.recipe, |recipe| {
            recipe(state, token, self.depth(), binding, discovered)
        });
        if entered {
            self.observed.set(installed);
        }
    }
}

#[cfg(test)]
/// Supplies the private single-entry fixture with real descendant attachment.
pub(super) struct ChildMounts<'a, R, A, P> {
    /// Root-to-parent path used to compose each test child route.
    path: P,
    /// Installed parent lifetime for each discovered test declaration.
    parent: ScopeToken,
    /// Parent depth before adding a child level.
    depth: usize,
    /// Existing weak binding; the fixture cannot create a separate actor.
    binding: &'a Binding<R, A>,
    /// Production discovery output used by the real roster traversal.
    discovered: &'a mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>,
}

#[cfg(test)]
impl<R: Send + 'static, A: Send + 'static, P: BorrowPath<R> + Send + Sync + 'static>
    ChildMounts<'_, R, A, P>
where
    P::State: 'static,
{
    ordinary_test_items! {
        /// Attaches a real output handler for test-util retirement and routing tests.
        #[cfg(feature = "test-util")]
        pub(super) fn output<O: Send + 'static>(
            &mut self,
            source: &MountSite<R, impl Send + 'static, impl Send + 'static, A>,
            configure: impl FnOnce(&mut super::super::output::OutputHandlers<R, A, O>),
        ) -> super::super::output::Output<O> {
            let (inner, output) = super::super::output::attach_owner(
                source.output_registration(),
                configure,
            );
            let owner = NestedOwner {
                inner,
                parent: self.parent,
                depth: self.depth.checked_add(1).expect("mount depth space exhausted"),
            };
            let ingress = self
                .binding
                .ingress
                .upgrade()
                .expect("reconciling actor has ingress");
            let lifecycle = self
                .binding
                .lifecycle
                .upgrade()
                .expect("reconciling actor has lifecycle");
            owner.bind(&ingress, &lifecycle);
            self.discovered.push(Box::new(owner));
            output
        }
    }

    /// Composes one optional test child and binds it to the existing actor.
    pub(super) fn optional<F, K>(
        &mut self,
        descriptor: OptionalMount<P::State, F, K>,
    ) -> MountSite<R, F::State, F::Action, A>
    where
        F: Feature + Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let OptionalMount {
            path,
            mut identity,
            make,
            initial,
            reactions,
        } = descriptor;
        let parent_path = self.path.clone();
        let descriptor = Descriptor {
            path: ComposedPath::new(self.path.clone(), path),
            identity: Box::new(move |root, live| {
                let parent = parent_path.read_structural(root, live)?;
                if !live() {
                    return None;
                }
                identity(parent, live)
            }),
            make,
            initial,
            reactions,
        };
        let (inner, site) = attach_descriptor(descriptor);
        let owner = NestedOwner {
            inner,
            parent: self.parent,
            depth: self
                .depth
                .checked_add(1)
                .expect("mount depth space exhausted"),
        };
        let ingress = self
            .binding
            .ingress
            .upgrade()
            .expect("reconciling actor has ingress");
        let lifecycle = self
            .binding
            .lifecycle
            .upgrade()
            .expect("reconciling actor has lifecycle");
        owner.bind(&ingress, &lifecycle);
        self.discovered.push(Box::new(owner));
        site
    }
}

/// Attaches one keyed entry for the private nested-ownership compiler fixture.
///
/// This helper exercises the real declaration and roster, not the public
/// collection authoring API or its multi-entry reconciliation algorithm.
#[cfg(test)]
#[expect(
    clippy::type_complexity,
    reason = "keep the paired shared declaration owner and typed mount site explicit in the test helper"
)]
pub(super) fn attach_selected_key<R, A, F, K, G>(
    read: for<'a> fn(&'a R) -> &'a crate::KeyedState<K, F::State>,
    write: for<'a> fn(&'a mut R) -> &'a mut crate::KeyedState<K, F::State>,
    key: K,
    make: impl FnMut(&F::State) -> F + Send + 'static,
    mut recipe: G,
) -> (
    Box<dyn MountDeclaration<R, A> + Send>,
    MountSite<R, F::State, F::Action, A>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Send + Sync + 'static,
    G: FnMut(&F::State, &mut ChildMounts<'_, R, A, KeyedPath<R, K, F::State, Arc<K>>>)
        + Send
        + 'static,
{
    let key = Arc::new(key);
    let path = KeyedPath::new(read, write, std::sync::Arc::clone(&key));
    let descriptor = Descriptor {
        path: path.clone(),
        identity: Box::new(move |root, live| {
            let collection = read(root);
            if !live() {
                return None;
            }
            collection.entry_stamp(&*key).map(|stamp| MountIdentity {
                key: stamp.to_owned(),
                replacement: 0,
                configuration: 0,
            })
        }),
        make: Box::new(make),
        initial: None,
        reactions: Vec::new(),
    };
    let (inner, site) = attach_descriptor(descriptor);
    let owner = RecipeOwner::new(
        inner,
        move |root: &R,
              parent,
              depth,
              binding: &Binding<R, A>,
              discovered: &mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>| {
            let live = || {
                binding
                    .lifecycle
                    .upgrade()
                    .is_some_and(|lifecycle| !lifecycle.is_closed())
            };
            let state = path.read_structural(root, &live);
            if !live() {
                return false;
            }
            let Some(state) = state else {
                return false;
            };
            recipe(
                state,
                &mut ChildMounts {
                    path: path.clone(),
                    parent,
                    depth,
                    binding,
                    discovered,
                },
            );
            true
        },
    );
    (Box::new(owner), site)
}
