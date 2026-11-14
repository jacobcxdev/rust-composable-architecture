//! Models ordered collection values and insertion lifetimes without using RCA collections.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Describes one operation shared by the real collection and independent model.
#[derive(Clone, Copy, Debug)]
pub enum CollectionOperation {
    /// Inserts a new key, or leaves an existing key unchanged.
    Insert { id: u8, value: i16 },
    /// Inserts a new key, or reports the rejected key and value.
    TryInsert { id: u8, value: i16 },
    /// Removes a key while preserving the order of all survivors.
    Remove { id: u8 },
    /// Removes a key by moving the last entry into its position.
    SwapRemove { id: u8 },
    /// Moves one entry between indices reduced to the current length.
    Move { from: u8, to: u8 },
    /// Mutates every visited value, then retains keys selected by parity.
    Retain { selector: u8, delta: i8 },
    /// Removes every current entry.
    Clear,
    /// Adds to one present value with signed wrapping arithmetic.
    Mutate { id: u8, delta: i8 },
    /// Saves a branch with the same current insertion identities.
    Clone,
    /// Restores a clone of the saved branch, if one exists.
    RestoreClone,
    /// Replaces the current collection with a distinct lineage.
    Replace { seed: u8 },
}

/// Distinguishes an insertion from other insertions and independent collections.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Lifetime {
    /// Identity shared only by a collection and its cloned branches.
    pub lineage: u64,
    /// Insertion number allocated across every branch of that lineage.
    pub revision: u64,
}

/// Distinguishes current child activation from earlier activations of the same insertion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogicalGeneration {
    /// Insertion identity visible in the current collection.
    pub lifetime: Lifetime,
    /// Activation number that changes after retirement and restoration.
    pub generation: u64,
}

/// Tracks current child activations independently of RCA runtime tokens.
#[derive(Debug)]
pub struct GenerationTracker {
    /// Current activation for each visible public key.
    active: HashMap<u8, LogicalGeneration>,
    /// Next activation number, never reused within one model history.
    next: u64,
}

impl GenerationTracker {
    /// Assigns activations to every entry in the initial model state.
    pub fn new(world: &ModelWorld) -> Self {
        let mut tracker = Self {
            active: HashMap::new(),
            next: 0,
        };
        tracker.reconcile(world);
        tracker
    }

    /// Preserves surviving activations and assigns fresh ones after identity changes.
    ///
    /// Restoring an old insertion after its retirement creates a new activation.
    /// A generation counter overflow is a programming error in the bounded caller.
    pub fn reconcile(&mut self, world: &ModelWorld) {
        let previous = std::mem::take(&mut self.active);
        for (id, lifetime) in world.lifetimes() {
            let generation = previous
                .get(&id)
                .filter(|entry| entry.lifetime == lifetime)
                .map_or_else(
                    || {
                        let generation = self.next;
                        self.next = self.next.checked_add(1).expect("bounded model generation");
                        generation
                    },
                    |entry| entry.generation,
                );
            let replaced = self.active.insert(
                id,
                LogicalGeneration {
                    lifetime,
                    generation,
                },
            );
            assert!(replaced.is_none());
        }
    }

    /// Returns the activation currently associated with `id`.
    pub fn get(&self, id: u8) -> Option<LogicalGeneration> {
        self.active.get(&id).copied()
    }

    /// Counts current activations rather than all historical insertions.
    pub fn len(&self) -> usize {
        self.active.len()
    }
}

/// Stores one model value beside its stable insertion identity.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    /// Public key used for linear lookup.
    id: u8,
    /// Current signed value.
    value: i16,
    /// Identity retained by moves, mutation and clones.
    lifetime: Lifetime,
}

/// Uses a vector to model ordered collection behaviour independently of RCA.
#[derive(Clone, Debug)]
struct Collection {
    /// Identity of this collection and its cloned branches.
    lineage: u64,
    /// Shared counter prevents different branches from reusing insertion identities.
    next_revision: Arc<AtomicU64>,
    /// Ordered values checked against the real keyed collection.
    entries: Vec<Entry>,
}

impl Collection {
    /// Starts an empty lineage with no allocated insertion identities.
    fn new(lineage: u64) -> Self {
        Self {
            lineage,
            next_revision: Arc::new(AtomicU64::new(1)),
            entries: Vec::new(),
        }
    }

    /// Appends a unique key with a new revision, or rejects a duplicate.
    fn insert(&mut self, id: u8, value: i16) -> bool {
        if self.entries.iter().any(|entry| entry.id == id) {
            return false;
        }
        let revision = self
            .next_revision
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |revision| {
                revision.checked_add(1)
            })
            .expect("bounded model revision");
        self.entries.push(Entry {
            id,
            value,
            lifetime: Lifetime {
                lineage: self.lineage,
                revision,
            },
        });
        true
    }

    /// Removes a matching entry without changing survivor order.
    fn remove(&mut self, id: u8) -> Option<i16> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.remove(index).value)
    }

    /// Removes a matching entry by moving the last entry into its position.
    fn swap_remove(&mut self, id: u8) -> Option<i16> {
        let index = self.entries.iter().position(|entry| entry.id == id)?;
        Some(self.entries.swap_remove(index).value)
    }
}

/// Records operation results for the public collection property tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionOutcome {
    /// The supplied key and value became a new entry.
    Inserted,
    /// The conditional insertion left an existing entry unchanged.
    SkippedDuplicate,
    /// A checked insertion returned the rejected owned inputs.
    Duplicate { id: u8, value: i16 },
    /// The removal returned the previous value, or found no key.
    Removed(Option<i16>),
    /// The operation took its applicable branch, even if values stayed equal.
    Changed,
    /// A missing prerequisite made the operation a no-op.
    Unchanged,
}

/// Holds the current collection, one saved branch and fresh-lineage counter.
#[derive(Clone, Debug)]
pub struct ModelWorld {
    /// Current ordered entries and their insertion identities.
    current: Collection,
    /// Saved branch whose identities survive cloning and restoration.
    saved: Option<Collection>,
    /// Distinct lineage assigned by the next replacement operation.
    next_lineage: u64,
}

impl ModelWorld {
    /// Starts an empty history without a saved clone.
    pub fn new() -> Self {
        Self {
            current: Collection::new(0),
            saved: None,
            next_lineage: 1,
        }
    }

    /// Applies one operation using vector lookup, order and explicit identity rules.
    pub fn apply(&mut self, operation: CollectionOperation) -> CollectionOutcome {
        match operation {
            CollectionOperation::Insert { id, value } => {
                if self.current.insert(id, value) {
                    CollectionOutcome::Inserted
                } else {
                    CollectionOutcome::SkippedDuplicate
                }
            }
            CollectionOperation::TryInsert { id, value } => {
                if self.current.insert(id, value) {
                    CollectionOutcome::Inserted
                } else {
                    CollectionOutcome::Duplicate { id, value }
                }
            }
            CollectionOperation::Remove { id } => {
                CollectionOutcome::Removed(self.current.remove(id))
            }
            CollectionOperation::SwapRemove { id } => {
                CollectionOutcome::Removed(self.current.swap_remove(id))
            }
            CollectionOperation::Move { from, to } => {
                if self.current.entries.is_empty() {
                    CollectionOutcome::Unchanged
                } else {
                    let len = self.current.entries.len();
                    let entry = self.current.entries.remove(usize::from(from) % len);
                    self.current.entries.insert(usize::from(to) % len, entry);
                    CollectionOutcome::Changed
                }
            }
            CollectionOperation::Retain { selector, delta } => {
                self.current.entries.retain_mut(|entry| {
                    entry.value = entry.value.wrapping_add(i16::from(delta));
                    (entry.id ^ selector) & 1 == 0
                });
                CollectionOutcome::Changed
            }
            CollectionOperation::Clear => {
                self.current.entries.clear();
                CollectionOutcome::Changed
            }
            CollectionOperation::Mutate { id, delta } => {
                if let Some(entry) = self.current.entries.iter_mut().find(|entry| entry.id == id) {
                    entry.value = entry.value.wrapping_add(i16::from(delta));
                    CollectionOutcome::Changed
                } else {
                    CollectionOutcome::Unchanged
                }
            }
            CollectionOperation::Clone => {
                self.saved = Some(self.current.clone());
                CollectionOutcome::Changed
            }
            CollectionOperation::RestoreClone => {
                if let Some(saved) = &self.saved {
                    self.current = saved.clone();
                    CollectionOutcome::Changed
                } else {
                    CollectionOutcome::Unchanged
                }
            }
            CollectionOperation::Replace { seed } => {
                let lineage = self.next_lineage;
                self.next_lineage = self
                    .next_lineage
                    .checked_add(1)
                    .expect("bounded model lineage");
                let mut replacement = Collection::new(lineage);
                for (id, value) in replacement_values(seed) {
                    assert!(replacement.insert(id, value));
                }
                self.current = replacement;
                CollectionOutcome::Changed
            }
        }
    }

    /// Copies current key/value pairs in their observable order.
    pub fn values(&self) -> Vec<(u8, i16)> {
        self.current
            .entries
            .iter()
            .map(|entry| (entry.id, entry.value))
            .collect()
    }

    /// Returns the insertion identity for a present key.
    pub fn lifetime(&self, id: u8) -> Option<Lifetime> {
        self.current
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.lifetime)
    }

    /// Copies current insertion identities in collection order.
    pub fn lifetimes(&self) -> Vec<(u8, Lifetime)> {
        self.current
            .entries
            .iter()
            .map(|entry| (entry.id, entry.lifetime))
            .collect()
    }

    /// Copies the saved branch's identities without restoring it.
    pub fn saved_lifetimes(&self) -> Option<Vec<(u8, Lifetime)>> {
        self.saved.as_ref().map(|saved| {
            saved
                .entries
                .iter()
                .map(|entry| (entry.id, entry.lifetime))
                .collect()
        })
    }
}

/// Produces at most eight distinct keys and signed values from one seed byte.
///
/// Adding three modulo eight visits every key before repeating. The real and
/// model collections receive these same input values but store them independently.
pub fn replacement_values(seed: u8) -> Vec<(u8, i16)> {
    let count = usize::from(seed % 9);
    (0..count)
        .map(|index| {
            let offset = u8::try_from(index).expect("replacement index is bounded");
            (
                seed.wrapping_add(offset.wrapping_mul(3)) & 7,
                i16::from(seed)
                    .wrapping_mul(17)
                    .wrapping_add(i16::from(offset)),
            )
        })
        .collect()
}
