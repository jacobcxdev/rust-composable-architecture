//! Caller-owned codecs over captured ordered pairs, never private runtime stamps.

use core::fmt;
use core::hash::{Hash, Hasher};
use std::backtrace::Backtrace;

use super::{KeyedIter, KeyedMutationError, KeyedState};

/// Reports caller decoding failure or rejected duplicate decoded identifiers.
pub struct KeyedDecodeError<Id, State, E> {
    /// Owns either decoded pairs or the codec error, never both or neither.
    kind: DecodeFailure<Id, State, E>,
    /// Retains the original validation or codec failure site when capture is enabled.
    backtrace: Backtrace,
}

/// Separates successful decoding with duplicate IDs from failure to decode.
enum DecodeFailure<Id, State, E> {
    /// Keeps every rejected pair in decoded order until recovery or drop.
    Duplicate(Vec<(Id, State)>),
    /// Keeps the caller error without formatting, cloning or erasing its type.
    Codec(E),
}

impl<Id, State, E> KeyedDecodeError<Id, State, E> {
    /// Recovers decoded pairs when their identifiers were not unique.
    #[must_use]
    pub fn into_inputs(self) -> Option<Vec<(Id, State)>> {
        match self.kind {
            DecodeFailure::Duplicate(inputs) => Some(inputs),
            DecodeFailure::Codec(_) => None,
        }
    }

    /// Recovers decoded pairs or the caller decoding error.
    #[must_use]
    pub fn into_parts(self) -> (Option<Vec<(Id, State)>>, Option<E>) {
        match self.kind {
            DecodeFailure::Duplicate(inputs) => (Some(inputs), None),
            DecodeFailure::Codec(cause) => (None, Some(cause)),
        }
    }

    /// Borrows the caller decoding error, if decoding itself failed.
    pub const fn codec_error(&self) -> Option<&E> {
        match &self.kind {
            DecodeFailure::Duplicate(_) => None,
            DecodeFailure::Codec(cause) => Some(cause),
        }
    }

    /// Returns the captured error backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl<Id, State, E> fmt::Debug for KeyedDecodeError<Id, State, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyedDecodeError")
            .field(
                "duplicate_identifiers",
                &matches!(self.kind, DecodeFailure::Duplicate(_)),
            )
            .field("backtrace", &self.backtrace)
            .finish_non_exhaustive()
    }
}
impl<Id, State, E> fmt::Display for KeyedDecodeError<Id, State, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "KeyedState decoding failed: {}\n{}",
            match self.kind {
                DecodeFailure::Duplicate(_) => "duplicate identifiers",
                DecodeFailure::Codec(_) => "caller codec error",
            },
            self.backtrace
        )
    }
}
impl<Id, State, E: std::error::Error + 'static> std::error::Error
    for KeyedDecodeError<Id, State, E>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.codec_error()
            .map(|cause| cause as &dyn std::error::Error)
    }
}

impl<Id, State> KeyedState<Id, State> {
    /// Encodes captured pairs in order through a caller-provided codec.
    ///
    /// The codec receives no private runtime stamps. It may borrow values without
    /// requiring `Clone`, `Debug`, `Send` or an element-derived identity function.
    /// # Errors
    /// Returns the caller's encoding error unchanged.
    /// # Panics
    /// Panics if the caller codec panics.
    pub fn encode_with<T, E>(
        &self,
        encode: impl FnOnce(KeyedIter<'_, Id, State>) -> Result<T, E>,
    ) -> Result<T, E> {
        encode(self.into_iter())
    }
}

impl<Id: Eq + Hash, State> KeyedState<Id, State> {
    /// Decodes captured ordered pairs into entirely fresh runtime authority.
    ///
    /// Public equality and hashes ignore runtime authority; decoding equal values
    /// does not resurrect any old child handle or pending effect output.
    /// # Errors
    /// Returns caller decoding errors, or all decoded pairs if IDs are duplicated.
    /// # Panics
    /// Panics if the caller codec or identifier hashing/equality panics, or revisions overflow.
    pub fn decode_with<T, E>(
        input: T,
        decode: impl FnOnce(T) -> Result<Vec<(Id, State)>, E>,
    ) -> Result<Self, KeyedDecodeError<Id, State, E>> {
        let pairs = decode(input).map_err(|cause| KeyedDecodeError {
            kind: DecodeFailure::Codec(cause),
            backtrace: Backtrace::capture(),
        })?;
        let mut states = Self::new();
        states
            .try_insert_many_at(0, pairs)
            .map_err(|error: KeyedMutationError<Id, State>| {
                let (inputs, backtrace) = error.into_inputs_and_backtrace();
                KeyedDecodeError {
                    kind: DecodeFailure::Duplicate(inputs),
                    backtrace,
                }
            })?;
        Ok(states)
    }
}

impl<Id: Hash, State: Hash> Hash for KeyedState<Id, State> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.len().hash(state);
        for pair in self {
            pair.hash(state);
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::KeyedState;

    #[test]
    fn decode_hash_unwind_releases_committed_and_uncommitted_pairs() {
        use std::cell::Cell;
        use std::hash::{Hash, Hasher};
        use std::rc::Rc;

        struct Id {
            value: Rc<u8>,
            hashes: Rc<Cell<usize>>,
        }
        impl PartialEq for Id {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }
        impl Eq for Id {}
        impl Hash for Id {
            fn hash<H: Hasher>(&self, state: &mut H) {
                let calls = self.hashes.get() + 1;
                self.hashes.set(calls);
                // Three validation hashes, then one committed insertion precede this unwind.
                assert_ne!(calls, 5, "deliberate decoded-key unwind");
                self.value.hash(state);
            }
        }

        let hashes = Rc::new(Cell::new(0));
        let inputs: Vec<_> = (0_u8..3)
            .map(|value| {
                (
                    Id {
                        value: Rc::new(value),
                        hashes: Rc::clone(&hashes),
                    },
                    Rc::new(value),
                )
            })
            .collect();
        let owners: Vec<_> = inputs
            .iter()
            .map(|(id, state)| (Rc::downgrade(&id.value), Rc::downgrade(state)))
            .collect();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = KeyedState::decode_with(inputs, Ok::<_, ()>);
        }));
        outcome.expect_err("the second decoded insertion must unwind");
        assert_eq!(hashes.get(), 5);
        for (id, state) in owners {
            assert!(id.upgrade().is_none());
            assert!(state.upgrade().is_none());
        }
    }
}
}
