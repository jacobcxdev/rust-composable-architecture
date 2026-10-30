//! Allocation and callback counts around the actual collection/DFS phases.
//!
//! State mutation, reconciliation, stale-node pruning and owner disposal have
//! separate samples. Counter snapshots and reports stay outside those samples.
use super::allocation_tests::measure;
use super::{
    Arc, Command, EntryMount, Feature, Hash, MountDeclaration, MountIdentity, OptionalMount,
    OptionalPath, ScopeIdentity, ScopeRegistry, ScopeSlot, ScopeTransitionBatch, Shared,
};
use crate::effect::lineage::ControlWake;
use crate::store::ingress::CommandIngress;
use std::cell::Cell;
use std::hash::Hasher;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};

std::thread_local! {
    /// Counts inspected descendants, moved interval boxes and scanned sibling tokens.
    ///
    /// Only allocation-measurement builds record these actor-thread operations.
    static METADATA: Cell<[usize; 3]> = const { Cell::new([0; 3]) };
}
/// Adds one production instrumentation event to its fixed metadata counter.
fn record(index: usize, count: usize) {
    METADATA.with(|value| {
        let mut counts = value.get();
        counts[index] += count;
        value.set(counts);
    });
}
/// Counts descendant positions inspected by real reconciliation.
pub(in crate::store) fn record_inspected(count: usize) {
    record(0, count);
}
/// Counts existing owner boxes moved between roster positions.
pub(in crate::store) fn record_moved(count: usize) {
    record(1, count);
}
/// Counts all sibling tokens scanned, including unrelated legacy siblings.
pub(in crate::store) fn record_ring(count: usize) {
    record(2, count);
}

#[derive(Default)]
/// Records key clones, parent construction, recipe calls, leaf construction and destruction.
struct Counts(
    /// Slots: key clone, parent make, recipe call, leaf make, parent/recipe/leaf drop.
    [AtomicUsize; 7],
);
impl Counts {
    /// Increments one fixed callback or destructor slot on the actor thread.
    fn add(&self, index: usize) {
        self.0[index].fetch_add(1, Ordering::Relaxed);
    }
    /// Reads cumulative counts before or after a synchronous phase.
    fn snapshot(&self) -> [usize; 7] {
        std::array::from_fn(|index| self.0[index].load(Ordering::Relaxed))
    }
}
/// Counts domain-key clones without counting shared probe-owner clones as domain work.
struct Key(usize, Arc<Counts>);
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Key {}
impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}
impl Clone for Key {
    fn clone(&self) -> Self {
        self.1.add(0);
        Self(self.0, std::sync::Arc::clone(&self.1))
    }
}
/// Supplies either one optional child or a two-entry nested collection.
struct Row {
    child: Option<usize>,
    nested: crate::KeyedState<Key, usize>,
}
/// Owns the ordered keyed rows mutated between reconciliation samples.
struct State {
    rows: crate::KeyedState<Key, Row>,
}
/// Makes parent construction, cache preservation and final destruction observable.
struct Parent {
    /// Shares callback and lifetime counters with all descendants.
    counts: Arc<Counts>,
    /// Remains owned by the same feature across retained-entry passes.
    cache: Cell<usize>,
}
impl Feature for Parent {
    type State = Row;
    type Action = u8;
    fn update(&mut self, _: &mut Row, _: &u8, _: &mut crate::FeatureContext<'_, Row, u8>) {
        self.cache.set(self.cache.get() + 1);
    }
}
impl Drop for Parent {
    fn drop(&mut self) {
        self.counts.add(4);
    }
}
/// Counts destruction of a captured recipe separately from its parent feature.
struct RecipeCapture(Arc<Counts>);
impl Drop for RecipeCapture {
    fn drop(&mut self) {
        self.0.add(5);
    }
}
/// Counts child-feature destruction without reducer-side state changes.
struct Leaf(Arc<Counts>);
impl Feature for Leaf {
    type State = usize;
    type Action = char;
    fn update(&mut self, _: &mut usize, _: &char, _: &mut crate::FeatureContext<'_, usize, char>) {}
}
impl Drop for Leaf {
    fn drop(&mut self) {
        self.0.add(6);
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
/// Selects no child, one optional child or two keyed children per row.
enum Profile {
    /// Keeps each parent feature without a child recipe.
    NoChildren,
    /// Installs one optional child through each row's recipe.
    ChildRecipe,
    /// Installs two keyed children through each row's recipe.
    NestedCollection,
}
/// Creates the literal row shape before reconciliation begins.
fn row(counts: &Arc<Counts>, profile: Profile) -> Row {
    Row {
        child: Some(1),
        nested: if profile == Profile::NestedCollection {
            (0..2)
                .map(|id| (Key(id, std::sync::Arc::clone(counts)), 1))
                .collect()
        } else {
            crate::KeyedState::new()
        },
    }
}
/// Checks full replacement, retained rows and sparse edits through the real roster.
#[expect(
    clippy::too_many_lines,
    reason = "keep the shared measured lifecycle and its per-phase cost assertions together"
)]
fn run(profile: Profile, count: usize, legacy: usize) {
    let counts = Arc::new(Counts::default());
    let (mut state, setup) = measure(|| State {
        rows: (0..count)
            .map(|id| {
                (
                    Key(id, std::sync::Arc::clone(&counts)),
                    row(&counts, profile),
                )
            })
            .collect(),
    });
    let make_counts = std::sync::Arc::clone(&counts);
    let (owner, attach) = measure(|| {
        let owner = super::children::keyed::<State, (), State, Parent, Key, _, _>(
            OptionalPath::new(|state| Some(state), |state| Some(state)),
            |state| &state.rows,
            |state| &mut state.rows,
            0,
            move |_, _, _| {
                make_counts.add(1);
                let feature = Parent {
                    counts: std::sync::Arc::clone(&make_counts),
                    cache: Cell::new(0),
                };
                let recipe = if profile == Profile::NoChildren {
                    None
                } else {
                    let capture = RecipeCapture(std::sync::Arc::clone(&make_counts));
                    let counts = std::sync::Arc::clone(&make_counts);
                    Some(
                        move |_: &Row,
                              children: &mut super::children::MountChildren<
                            '_,
                            State,
                            (),
                            Row,
                            u8,
                        >| {
                            let _ = &capture;
                            counts.add(2);
                            let counts = std::sync::Arc::clone(&counts);
                            if profile == Profile::NestedCollection {
                                children.keyed(
                                    |row| &row.nested,
                                    |row| &mut row.nested,
                                    move |_, _| {
                                        counts.add(3);
                                        EntryMount::new(Leaf(std::sync::Arc::clone(&counts)))
                                    },
                                );
                            } else {
                                children.optional(OptionalMount::new(
                                    |row: &Row| row.child.as_ref(),
                                    |row: &mut Row| row.child.as_mut(),
                                    |row: &Row| {
                                        row.child.map(|_| MountIdentity {
                                            key: (),
                                            replacement: 0,
                                            configuration: 0,
                                        })
                                    },
                                    move |_| {
                                        counts.add(3);
                                        Leaf(std::sync::Arc::clone(&counts))
                                    },
                                ));
                            }
                        },
                    )
                };
                (EntryMount::new(feature), recipe)
            },
        );
        Box::new(owner) as Box<dyn MountDeclaration<State, ()> + Send>
    });
    let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<State, ()>>::new(
        sender,
        ControlWake::new(),
    ));
    let (lifecycle, mut scopes) = ScopeRegistry::for_test();
    owner.bind(&ingress, &lifecycle);
    for _ in 0..legacy {
        scopes.mount(
            scopes.root(),
            ScopeRegistry::node(&mut ScopeSlot::new()),
            ScopeIdentity::unit(),
        );
    }
    let mut mounts = vec![owner];
    let mut nodes = Vec::new();
    eprintln!(
        "keyed {profile:?} entries={count} legacy={legacy} setup={setup:?} attach={attach:?}"
    );
    let mut sparse_identities = Vec::new();
    let mut sparse_scopes = Vec::new();
    for phase in 0..=8 {
        if phase == 6 && profile != Profile::NestedCollection {
            continue;
        }
        if phase >= 7 && (count != 64 || profile == Profile::NestedCollection) {
            break;
        }
        if phase == 7 {
            sparse_identities = state
                .rows
                .entry_stamps()
                .zip(mounts.iter().filter(|owner| owner.depth() == 1))
                .map(|((key, stamp), owner)| {
                    assert!(owner.position().entry_stamp.unwrap().matches(stamp));
                    (key.0, stamp.to_owned(), owner.installed_scope().unwrap())
                })
                .collect();
            assert_eq!(sparse_identities.len(), count);
            sparse_scopes = mounts
                .iter()
                .filter_map(|owner| owner.installed_scope())
                .collect();
        }
        let before_mutation = counts.snapshot();
        let ((), mutation) = measure(|| match phase {
            2 => state.rows.reverse(),
            4 => {
                state.rows = (0..count)
                    .map(|id| {
                        (
                            Key(id, std::sync::Arc::clone(&counts)),
                            row(&counts, profile),
                        )
                    })
                    .collect();
            }
            6 => {
                for (_, row) in state.rows.iter_mut() {
                    row.nested.reverse();
                    row.nested.remove(&Key(0, std::sync::Arc::clone(&counts)));
                    row.nested.insert(Key(0, std::sync::Arc::clone(&counts)), 2);
                }
            }
            7 => {
                state
                    .rows
                    .remove(&Key(count / 2, std::sync::Arc::clone(&counts)))
                    .expect("middle row exists before sparse removal");
            }
            8 => {
                state.rows.insert(
                    Key(count / 2, std::sync::Arc::clone(&counts)),
                    row(&counts, profile),
                );
            }
            _ => {}
        });
        let before = counts.snapshot();
        let mutation_counts: [usize; 7] =
            std::array::from_fn(|index| before[index] - before_mutation[index]);
        METADATA.with(|value| value.set([0; 3]));
        let mut transitions = ScopeTransitionBatch::default();
        let ((), reconcile) = measure(|| {
            super::nested::reconcile_roster::<State, (), Shared>(
                &mut mounts,
                true,
                &state,
                &mut scopes,
                &mut transitions,
                &mut nodes,
                #[cfg(feature = "diagnostics")]
                None,
            );
        });
        let metadata = METADATA.with(Cell::get);
        let ((), prune) = measure(|| {
            if !transitions.is_empty() {
                nodes.retain(|node| scopes.is_current(node.scope()));
            }
        });
        let after = counts.snapshot();
        let delta: [usize; 7] = std::array::from_fn(|index| after[index] - before[index]);
        if matches!(phase, 1 | 2 | 3 | 5) {
            assert_eq!(
                delta, [0; 7],
                "retained entries do not clone keys, reconstruct F/G/routes or destroy owners"
            );
        }
        if matches!(phase, 0 | 4) {
            let children = match profile {
                Profile::NoChildren => 0,
                Profile::ChildRecipe => count,
                Profile::NestedCollection => 2 * count,
            };
            assert_eq!(
                delta[0],
                count
                    + if profile == Profile::NestedCollection {
                        children
                    } else {
                        0
                    }
            );
            assert_eq!(delta[1], count);
            assert_eq!(
                delta[2],
                if profile == Profile::NoChildren {
                    0
                } else {
                    count
                }
            );
            assert_eq!(delta[3], children);
            if phase == 4 {
                assert_eq!(
                    &delta[4..],
                    &[
                        count,
                        if profile == Profile::NoChildren {
                            0
                        } else {
                            count
                        },
                        children
                    ]
                );
            }
        }
        if phase == 6 {
            assert_eq!(delta, [count, 0, 0, count, 0, 0, count]);
        }
        if matches!(phase, 7 | 8) {
            let children = usize::from(profile == Profile::ChildRecipe);
            assert_eq!(mutation_counts, [0; 7]);
            assert_eq!(
                delta,
                if phase == 7 {
                    [0, 0, 0, 0, 1, children, children]
                } else {
                    [1, 1, children, children, 0, 0, 0]
                },
                "sparse edits rebuild or destroy only the selected row"
            );
            assert!(
                state.rows.iter().map(|(key, _)| key.0).eq((0..count)
                    .filter(|id| *id != count / 2)
                    .chain((phase == 8).then_some(count / 2)))
            );
            assert_eq!(
                mounts.iter().filter(|owner| owner.depth() == 1).count(),
                state.rows.len()
            );
            for ((key, stamp), owner) in state
                .rows
                .entry_stamps()
                .zip(mounts.iter().filter(|owner| owner.depth() == 1))
            {
                let (_, old_stamp, old_scope) = sparse_identities
                    .iter()
                    .find(|(id, _, _)| *id == key.0)
                    .unwrap();
                assert!(owner.position().entry_stamp.unwrap().matches(stamp));
                if key.0 == count / 2 {
                    assert!(
                        !old_stamp.matches(stamp),
                        "reinsertion has a fresh generation"
                    );
                    assert_ne!(owner.installed_scope(), Some(*old_scope));
                } else {
                    assert!(old_stamp.matches(stamp));
                    assert_eq!(owner.installed_scope(), Some(*old_scope));
                }
            }
            let mut retired_scopes = sparse_scopes
                .iter()
                .copied()
                .filter(|token| !scopes.is_current(*token));
            assert_eq!(retired_scopes.clone().count(), 1 + children);
            if phase == 7 {
                assert!(retired_scopes.all(|token| transitions.contains(token)));
            } else {
                assert!(transitions.is_empty());
            }
            let (_, _, removed_scope) = sparse_identities
                .iter()
                .find(|(id, _, _)| *id == count / 2)
                .unwrap();
            assert!(!scopes.is_current(*removed_scope));
            assert!(nodes.iter().all(|node| scopes.is_current(node.scope())));
        }
        if phase == 2 && count > 1 {
            assert!(
                metadata[2] >= count + legacy,
                "full mixed sibling scan is measured"
            );
        }
        if matches!(phase, 1 | 3 | 5) {
            assert_eq!(
                metadata[1..],
                [0, 0],
                "unchanged passes neither move intervals nor scan sibling rings"
            );
        }
        eprintln!(
            "keyed {profile:?} entries={count} legacy={legacy} phase={phase} [0 cold,1 unchanged,2 reorder,3 unchanged,4 all replaced,5 unchanged,6 inner replacement+reorder,7 middle removal,8 append reinsertion] mutation={mutation:?} mutation-counters={mutation_counts:?}; actual reconcile={reconcile:?} prune={prune:?}; counts [K clone,F make,G call,leaf make,F drop,G drop,leaf drop]={delta:?}; metadata [descendants inspected,interval boxes moved,full sibling tokens scanned]={metadata:?}; roster len/cap={}/{}",
            mounts.len(),
            mounts.capacity()
        );
    }
    let before = counts.snapshot();
    let ((), disposal) = measure(|| drop((mounts, nodes)));
    let after = counts.snapshot();
    let delta: [usize; 7] = std::array::from_fn(|index| after[index] - before[index]);
    assert_eq!(delta[4], count);
    assert_eq!(
        delta[5],
        if profile == Profile::NoChildren {
            0
        } else {
            count
        }
    );
    eprintln!(
        "keyed {profile:?} entries={count} legacy={legacy} disposal={disposal:?} counters={delta:?}"
    );
}

/// Runs isolated collection samples and checks exact callback and owner changes.
pub(crate) fn keyed_collection_costs() {
    if crate::allocation_test::run_in_child(
        "store::mount::collection_costs::keyed_collection_costs",
    ) {
        return;
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
    let parent = scopes.root();
    let child = scopes.mount(
        parent,
        ScopeRegistry::node(&mut ScopeSlot::new()),
        ScopeIdentity::unit(),
    );
    for desired in [&[][..], &[child][..]] {
        let ((), costs) = measure(|| scopes.relink_native_children(parent, desired));
        eprintln!(
            "keyed direct relink selected={} actual={costs:?}",
            desired.len()
        );
        assert_eq!(
            costs,
            (0, 0),
            "empty and singleton relinks need no membership storage"
        );
    }
    for count in [1, 64] {
        for profile in [Profile::NoChildren, Profile::ChildRecipe] {
            run(profile, count, 0);
        }
    }
    run(Profile::ChildRecipe, 4, 3);
    run(Profile::NestedCollection, 2, 0);
    drop(profiler);
}
