//! Provides explicit checked embedding of owned, type-erased case payloads.
//!
//! This boundary is separate from typed borrowed paths. It checks Rust types,
//! not enum layout, diagnostic labels, or runtime scope identity.

use std::any::{Any, TypeId};
use std::backtrace::Backtrace;
use std::fmt;

/// Consumes a matching payload or returns the original rejected allocation.
type Embed = dyn Fn(Box<dyn Any>) -> Result<Box<dyn Any>, Box<dyn Any>>;

/// Embeds owned erased payloads into one explicitly selected root case.
///
/// Construction stores a boxed embedding closure. Neither construction nor use
/// adds bounds to typed borrowed paths. Payloads need not be `Clone` or `Send`.
/// Payloads must satisfy `Any`: they cannot contain non-static borrowed data.
/// Prefer typed paths when the root and payload types are known to the caller.
///
/// # Examples
/// ```
/// use rca_core::OwnedCasePath;
/// enum Event { Value(Option<String>) }
/// let path = OwnedCasePath::optional(Event::Value);
/// let event = path.try_embed::<Event>(Box::new(String::from("value"))).unwrap();
/// assert!(matches!(event, Event::Value(Some(value)) if value == "value"));
/// ```
pub struct OwnedCasePath {
    /// Rejects a different requested root before the embedding closure runs.
    root: TypeId,
    /// Boxes precisely the root type whose identity is stored alongside it.
    embed: Box<Embed>,
}

impl OwnedCasePath {
    /// Creates an adapter accepting only the exact owned payload type.
    pub fn new<Root: Any, Payload: Any>(embed: impl Fn(Payload) -> Root + 'static) -> Self {
        Self {
            root: TypeId::of::<Root>(),
            embed: Box::new(move |input| {
                input
                    .downcast::<Payload>()
                    .map(|payload| Box::new(embed(*payload)) as Box<dyn Any>)
            }),
        }
    }

    /// Creates an optional adapter explicitly supporting bare-value promotion.
    ///
    /// Exact `Option<T>` inputs, including `None`, are tried first. Only a bare
    /// `T` may otherwise be promoted into `Some(T)`; unrelated types are rejected.
    pub fn optional<Root: Any, T: Any>(embed: impl Fn(Option<T>) -> Root + 'static) -> Self {
        Self {
            root: TypeId::of::<Root>(),
            embed: Box::new(move |input| {
                let payload = match input.downcast::<Option<T>>() {
                    Ok(payload) => *payload,
                    Err(input) => Some(*input.downcast::<T>()?),
                };
                Ok(Box::new(embed(payload)))
            }),
        }
    }

    /// Checks the requested root, then embeds a compatible owned payload.
    ///
    /// # Errors
    /// Returns the original owned input when the requested root or payload type
    /// is incompatible. A root mismatch never invokes the embedding closure.
    ///
    /// # Panics
    /// Panics if the caller's embedding closure panics.
    pub fn try_embed<Root: Any>(&self, input: Box<dyn Any>) -> Result<Root, OwnedCaseMismatch> {
        if TypeId::of::<Root>() != self.root {
            return Err(OwnedCaseMismatch::new(input, true));
        }
        let root = (self.embed)(input).map_err(|input| OwnedCaseMismatch::new(input, false))?;
        // Constructors box precisely Root, and the requested type was checked above.
        Ok(*root
            .downcast::<Root>()
            .expect("checked owned case root type"))
    }
}

impl fmt::Debug for OwnedCasePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedCasePath")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

/// Retains an owned input rejected by checked case embedding.
pub struct OwnedCaseMismatch {
    /// Retains the caller's exact allocation until recovery or destruction.
    input: Box<dyn Any>,
    /// Distinguishes the early root check from rejection inside the payload adapter.
    root_mismatch: bool,
    /// Records the rejection site when Rust backtrace capture is enabled.
    backtrace: Backtrace,
}

impl OwnedCaseMismatch {
    /// Keeps rejected input owned while recording the failed type check.
    fn new(input: Box<dyn Any>, root_mismatch: bool) -> Self {
        Self {
            input,
            root_mismatch,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns the trace captured when this mismatch occurred.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    /// Reports whether the requested root type was incompatible.
    pub const fn is_root_mismatch(&self) -> bool {
        self.root_mismatch
    }

    /// Reports whether the owned payload type was incompatible.
    pub const fn is_payload_mismatch(&self) -> bool {
        !self.root_mismatch
    }

    /// Returns the rejected input without cloning or converting it.
    pub fn into_input(self) -> Box<dyn Any> {
        self.input
    }
}

impl fmt::Debug for OwnedCaseMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedCaseMismatch")
            .field("root_mismatch", &self.root_mismatch)
            .field("backtrace", &self.backtrace)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for OwnedCaseMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{}",
            if self.root_mismatch {
                "requested root type does not match the owned case path"
            } else {
                "owned payload type does not match the case path"
            }
        )?;
        fmt::Display::fmt(&self.backtrace, f)
    }
}

impl std::error::Error for OwnedCaseMismatch {}
