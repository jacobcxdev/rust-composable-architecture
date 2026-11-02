//! Keyed interval reconciliation within the existing actor-owned preorder roster.
//!
//! Stable entry stamps retain whole subtrees across reordering. Departing direct
//! entries retire before successor construction. Retained nested collections
//! reconcile later during depth-first traversal, not in a global retirement pass.
//! Unchanged prefixes and suffixes avoid rebuilding their membership maps.

use super::nested::{Descendants, NestedOwner};
use super::{
    Arc, Batch, Binding, BorrowPath, Carriers, Command, Lifecycle, MountDeclaration, MountPosition,
    OnceLock, ScopeRegistry, ScopeToken, ScopeTransitionBatch, with_payload,
};
use crate::composition::{CollectionStamp, EntryStamp};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Keeps the direct entry inline so leaf rows need no descendant allocation.
type Subtree<M> = (Box<M>, Vec<Box<M>>);

/// Leases collection construction and completion state outside its metadata borrow.
struct Payload<M> {
    /// Creates one fresh direct entry after departing subtrees are retired.
    make: M,
    /// Identifies the structural roster whose descendant traversal finished.
    completed: Option<CollectionStamp>,
    /// Awaits successful descendant traversal before becoming the completed stamp.
    pending: Option<CollectionStamp>,
}

/// Owns one keyed interval, while descendant declarations own their Features.
pub(super) struct Collection<R, A, C: Carriers<R, A>, P: BorrowPath<R>, K, S, M> {
    /// Releases owned callbacks before their borrowing-route metadata.
    payload: RefCell<Option<Payload<M>>>,
    /// Selects the parent without retaining its State.
    path: P,
    /// Selects the current keyed collection from that parent borrow.
    read: for<'a> fn(&'a P::State) -> &'a crate::KeyedState<K, S>,
    /// Separates direct entries from deeper descendants in the preorder roster.
    depth: usize,
    /// Binds this declaration once to weak root admission endpoints.
    binding: OnceLock<Binding<R, A, C>>,
}

impl<R, A, C: Carriers<R, A>, P: BorrowPath<R>, K, S, M> Collection<R, A, C, P, K, S, M> {
    /// Retains selectors and construction without selecting State or running callbacks.
    pub(super) fn new(
        path: P,
        read: for<'a> fn(&'a P::State) -> &'a crate::KeyedState<K, S>,
        depth: usize,
        make: M,
    ) -> Self {
        Self {
            path,
            read,
            depth,
            payload: RefCell::new(Some(Payload {
                make,
                completed: None,
                pending: None,
            })),
            binding: OnceLock::new(),
        }
    }
}

/// Partitions whole subtrees, retiring departures before returning retained ownership.
///
/// Input starts at a direct entry and contains complete preorder subtrees. All
/// departing scope tokens expire before any corresponding destructor runs.
/// Destruction is reverse preorder; a panic stops the actor, without restoration.
#[expect(
    clippy::mutable_key_type,
    reason = "EntryStamp hashes allocation identity and immutable revision, not its lineage atomic"
)]
fn retain_current_subtrees<R, A, C: Carriers<R, A>>(
    old: Vec<Box<C::Mount>>,
    wanted: &HashSet<EntryStamp>,
    scopes: &mut ScopeRegistry,
    transitions: &mut ScopeTransitionBatch,
    #[cfg(feature = "diagnostics")] mut lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
) -> HashMap<EntryStamp, Subtree<C::Mount>> {
    let mut retained = HashMap::new();
    let mut entries = old.into_iter().peekable();
    let mut departing = Vec::new();
    while let Some(entry) = entries.next() {
        let position = entry.position();
        let depth = position.depth;
        let stamp = position
            .entry_stamp
            .expect("collection interval starts with an entry")
            .clone();
        let keep = wanted.contains(&stamp)
            && entry
                .installed_scope()
                .is_some_and(|token| scopes.is_current(token));
        let mut descendants = Vec::new();
        while entries.peek().is_some_and(|child| child.depth() > depth) {
            descendants.push(entries.next().expect("peeked descendant"));
        }
        let subtree = (entry, descendants);
        if keep {
            retained.insert(stamp, subtree);
        } else {
            departing.push(subtree);
        }
    }
    // Retire every departing direct-entry subtree before any new make.
    for (entry, _) in &departing {
        if let Some(token) = entry.installed_scope() {
            let retired = scopes.leave(token);
            #[cfg(feature = "diagnostics")]
            if let Some(call) = &mut lifecycle {
                for token in &retired {
                    call.emit(crate::diagnostics::LifecycleKind::Retired {
                        lifetime: token.diagnostic_id(),
                    });
                }
            }
            transitions.extend(retired);
        }
    }
    for (entry, descendants) in departing.into_iter().rev() {
        for owner in std::iter::once(entry).chain(descendants).rev() {
            drop(owner);
        }
    }
    retained
}

impl<R, A, C: Carriers<R, A>, P: BorrowPath<R>, K, S, M> MountDeclaration<R, A, C>
    for Collection<R, A, C, P, K, S, M>
where
    M: FnMut(&K, &S, EntryStamp, ScopeToken, usize, &Binding<R, A, C>) -> Option<Box<C::Mount>>,
{
    fn can_discover(&self) -> bool {
        true
    }

    fn can_reorder(&self) -> bool {
        true
    }

    fn depth(&self) -> usize {
        self.depth
    }

    fn position(&self) -> MountPosition<'_> {
        MountPosition {
            depth: self.depth,
            parent: None,
            entry_stamp: None,
        }
    }

    fn bind(
        &self,
        ingress: &Arc<super::super::ingress::CommandIngress<Command<R, A, C>>>,
        lifecycle: &Arc<Lifecycle>,
    ) {
        assert!(
            self.binding
                .set(Binding {
                    ingress: Arc::downgrade(ingress),
                    lifecycle: Arc::downgrade(lifecycle)
                })
                .is_ok(),
            "collection binds one actor"
        );
    }

    fn finish_reconcile(&self) {
        let mut payload = self.payload.borrow_mut();
        let payload = payload
            .as_mut()
            .expect("reconciliation restores collection payload");
        if let Some(stamp) = payload.pending.take() {
            payload.completed = Some(stamp);
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "keep subtree partitioning, retirement and successor installation in one ownership sequence"
    )]
    fn reconcile(
        &self,
        state: &R,
        scopes: &mut ScopeRegistry,
        transitions: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<C::Node>>,
        parent: ScopeToken,
        discovered: &mut Vec<Box<C::Mount>>,
        retire_descendants: &mut Descendants<'_, R, A, C>,
        #[cfg(feature = "diagnostics")] lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        let Some(state) = self
            .path
            .read_structural(state, &|| scopes.installation_is_live())
        else {
            return;
        };
        if !scopes.installation_is_live() {
            return;
        }
        let states = (self.read)(state);
        if !scopes.installation_is_live() {
            return;
        }
        with_payload(&self.payload, |payload| {
            let stamp = states.structural_stamp();
            if payload
                .completed
                .as_ref()
                .is_some_and(|old| old.matches(stamp))
                && !retire_descendants
                    .inspect()
                    .iter()
                    .filter(|owner| owner.depth() == self.depth + 1)
                    .any(|owner| {
                        owner
                            .installed_scope()
                            .is_none_or(|token| !scopes.is_current(token))
                    })
            {
                return;
            }
            payload.completed = None;
            payload.pending = None;
            let mut old = retire_descendants.take();
            retire_descendants.reordered = true;
            // Match whole current subtrees at either end. The remaining window
            // alone needs membership storage and per-entry descendant vectors.
            let matches = |owner: &C::Mount, stamp| {
                owner
                    .position()
                    .entry_stamp
                    .expect("collection interval starts with an entry")
                    .matches(stamp)
                    && owner
                        .installed_scope()
                        .is_some_and(|token| scopes.is_current(token))
            };
            let mut heads = old
                .iter()
                .enumerate()
                .filter(|(_, owner)| owner.depth() == self.depth + 1)
                .peekable();
            let mut stamps = states.entry_stamps();
            let mut prefix_count = 0;
            while let (Some((_, owner)), Some((_, stamp))) = (heads.peek(), stamps.next()) {
                if !matches(owner.as_ref(), stamp) {
                    break;
                }
                prefix_count += 1;
                heads.next();
            }
            let start = match heads.peek() {
                Some((index, _)) => *index,
                None => old.len(),
            };
            let mut end = old.len();
            let mut suffix_count = 0;
            for ((index, owner), (_, stamp)) in heads.rev().zip(
                states
                    .entry_stamps()
                    .rev()
                    .take(states.len() - prefix_count),
            ) {
                if !matches(owner.as_ref(), stamp) {
                    break;
                }
                suffix_count += 1;
                end = index;
            }
            let middle_count = states.len() - prefix_count - suffix_count;
            let (prefix, old, suffix) = if prefix_count == 0 && suffix_count == 0 {
                (Vec::new(), old, Vec::new())
            } else {
                let suffix = old.split_off(end);
                let middle = old.split_off(start);
                (old, middle, suffix)
            };
            // Retaining the lineage allocation prevents ABA; equality/hash use
            // only its allocation identity and the immutable entry revision.
            #[expect(
                clippy::mutable_key_type,
                reason = "EntryStamp hashes allocation identity and immutable revision, not its lineage atomic"
            )]
            let wanted: HashSet<_> = states
                .entry_stamps()
                .skip(prefix_count)
                .take(middle_count)
                .map(|(_, stamp)| stamp.to_owned())
                .collect();
            #[expect(
                clippy::mutable_key_type,
                reason = "EntryStamp hashes allocation identity and immutable revision, not its lineage atomic"
            )]
            let mut retained = retain_current_subtrees::<R, A, C>(
                old,
                &wanted,
                scopes,
                transitions,
                #[cfg(feature = "diagnostics")]
                lifecycle,
            );
            if !scopes.installation_is_live() {
                return;
            }
            let binding = self
                .binding
                .get()
                .expect("actor binds collection before reconciliation");
            discovered.extend(prefix);
            for ((key, stamp), (_, state)) in states
                .entry_stamps()
                .zip(states.iter())
                .skip(prefix_count)
                .take(middle_count)
            {
                let stamp = stamp.to_owned();
                if let Some((entry, descendants)) = retained.remove(&stamp) {
                    discovered.push(entry);
                    discovered.extend(descendants);
                } else {
                    let Some(owner) =
                        (payload.make)(key, state, stamp, parent, self.depth + 1, binding)
                    else {
                        return;
                    };
                    if !scopes.installation_is_live() {
                        drop(owner);
                        return;
                    }
                    discovered.push(owner);
                }
            }
            discovered.extend(suffix);
            payload.pending = Some(states.structural_stamp().to_owned());
        });
    }
}

/// Adds immutable insertion identity without changing the entry's declaration protocol.
pub(super) struct Entry<I> {
    /// Owns the entry Feature, callbacks and optional nested recipe.
    pub(super) inner: I,
    /// Keeps the lineage allocation alive and prevents address-reuse identity matches.
    pub(super) stamp: EntryStamp,
}

impl<R, A, C: Carriers<R, A>, I: MountDeclaration<R, A, C>> MountDeclaration<R, A, C> for Entry<I> {
    fn installed_scope(&self) -> Option<ScopeToken> {
        self.inner.installed_scope()
    }

    fn position(&self) -> MountPosition<'_> {
        MountPosition {
            depth: 0,
            parent: None,
            entry_stamp: Some(&self.stamp),
        }
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
        frame: &mut super::reaction::Frame<R, Batch<R, A, C>>,
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

/// Binds a stamped entry before adding it beneath its installed parent.
pub(super) fn bind_entry<R, A, C: Carriers<R, A>, I: MountDeclaration<R, A, C>>(
    inner: I,
    stamp: EntryStamp,
    parent: ScopeToken,
    depth: usize,
    binding: &Binding<R, A, C>,
) -> NestedOwner<Entry<I>> {
    let owner = NestedOwner {
        inner: Entry { inner, stamp },
        parent,
        depth,
    };
    let ingress = binding
        .ingress
        .upgrade()
        .expect("reconciling actor has ingress");
    let lifecycle = binding
        .lifecycle
        .upgrade()
        .expect("reconciling actor has lifecycle");
    owner.bind(&ingress, &lifecycle);
    owner
}
