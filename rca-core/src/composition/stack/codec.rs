//! Converts ordered stack values without persisting logical IDs or runtime authority.
//!
//! Applications supply a closed route schema and own byte framing.
//! All routes must decode before RCA creates fresh stack identities.

use core::fmt;
use std::backtrace::Backtrace;

use super::StackState;

/// Initial envelope format: ordered route records without IDs or runtime stamps.
const FORMAT_VERSION: u32 = 1;

/// Carries one explicitly tagged application route payload.
///
/// The caller codec must reject unknown tags, unsupported route versions and
/// invalid or trailing payload bytes. No runtime type-name lookup is performed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedStackRoute {
    /// Stable application-owned route tag, never a runtime type name.
    pub tag: String,
    /// Application-owned version of this route's payload schema.
    pub version: u32,
    /// Complete encoded payload validated by the caller's route decoder.
    pub payload: Vec<u8>,
}

/// Carries a versioned ordered sequence of route values.
///
/// Version one is accepted. Logical IDs and private entry generations are never
/// encoded. Byte framing is caller-owned; an alternating raw tag/payload format
/// must reject odd input before constructing these complete typed records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackEncoding {
    /// Envelope format version, independently checked before route decoding.
    pub version: u32,
    /// Route records in their original stack order.
    pub routes: Vec<EncodedStackRoute>,
}

/// Reports an unsupported envelope or an indexed caller route decoding failure.
///
/// `Debug` and `Display` omit the original error to avoid printing application payloads.
/// Use [`Self::codec_error`] or [`std::error::Error::source`] for explicit cause access.
pub struct StackDecodeError<E> {
    /// Keeps a route's index and cause together.
    /// `None` means an unsupported envelope.
    route_failure: Option<(usize, E)>,
    /// Records the decoder boundary without retaining route data.
    backtrace: Backtrace,
}

impl<E> StackDecodeError<E> {
    /// Captures an envelope failure before any caller codec runs.
    fn version() -> Self {
        Self {
            route_failure: None,
            backtrace: Backtrace::capture(),
        }
    }

    /// Keeps the rejected route's index and original caller error together.
    fn route(index: usize, cause: E) -> Self {
        Self {
            route_failure: Some((index, cause)),
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether envelope version validation failed before decoding routes.
    #[must_use]
    pub const fn is_unsupported_version(&self) -> bool {
        self.route_failure.is_none()
    }

    /// Returns the zero-based position of the rejected route, if applicable.
    #[must_use]
    pub const fn route_index(&self) -> Option<usize> {
        match &self.route_failure {
            Some((index, _)) => Some(*index),
            None => None,
        }
    }

    /// Borrows the original caller decoding error, if applicable.
    #[must_use]
    pub const fn codec_error(&self) -> Option<&E> {
        match &self.route_failure {
            Some((_, cause)) => Some(cause),
            None => None,
        }
    }

    /// Returns the original caller decoding error, if applicable.
    #[must_use]
    pub fn into_codec_error(self) -> Option<E> {
        self.route_failure.map(|(_, cause)| cause)
    }

    /// Returns the captured error backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl<E> fmt::Debug for StackDecodeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StackDecodeError")
            .field("route_index", &self.route_index())
            .field("backtrace", &self.backtrace)
            .finish_non_exhaustive()
    }
}

impl<E> fmt::Display for StackDecodeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.route_index() {
            Some(index) => write!(
                f,
                "stack route decoding failed at index {index}\n{}",
                self.backtrace
            ),
            None => write!(f, "unsupported stack envelope version\n{}", self.backtrace),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for StackDecodeError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.codec_error()
            .map(|error| error as &dyn std::error::Error)
    }
}

impl<State> StackState<State> {
    /// Encodes borrowed route values in order without IDs or runtime generations.
    ///
    /// # Errors
    ///
    /// Returns the caller encoding error unchanged. No partial envelope is returned.
    ///
    /// # Panics
    ///
    /// Propagates caller codec panics.
    pub fn encode_with<E>(
        &self,
        encode: impl FnMut(&State) -> Result<EncodedStackRoute, E>,
    ) -> Result<StackEncoding, E> {
        Ok(StackEncoding {
            version: FORMAT_VERSION,
            routes: self.values().map(encode).collect::<Result<_, E>>()?,
        })
    }

    /// Decodes ordered values into entirely fresh logical IDs and runtime authority.
    ///
    /// The closed application decoder validates every tag, route version and full
    /// payload. Every route must succeed before fresh stack construction begins.
    /// No partial stack, skipped route or identity-preserving decode is provided.
    /// Installing the returned stack is an explicit caller action. Equal payloads
    /// do not resurrect old scoped handles, timers, outputs or dismissal clients.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{EncodedStackRoute, StackEncoding, StackState};
    ///
    /// let encoded = StackEncoding {
    ///     version: 1,
    ///     routes: vec![EncodedStackRoute {
    ///         tag: "job".into(),
    ///         version: 1,
    ///         payload: vec![7],
    ///     }],
    /// };
    /// let jobs = StackState::decode_with(encoded, |route| {
    ///     match (route.tag.as_str(), route.version, route.payload.as_slice()) {
    ///         ("job", 1, [id]) => Ok(*id),
    ///         _ => Err("unknown or invalid job route"),
    ///     }
    /// })
    /// .unwrap();
    /// assert_eq!(jobs.values().copied().collect::<Vec<_>>(), [7]);
    /// ```
    ///
    /// # Errors
    ///
    /// Rejects unsupported envelope versions before calling the decoder. Caller
    /// errors retain the failing route index and original typed cause.
    ///
    /// # Panics
    ///
    /// Propagates caller codec panics. Private entry revision overflow also panics.
    pub fn decode_with<E>(
        input: StackEncoding,
        mut decode: impl FnMut(EncodedStackRoute) -> Result<State, E>,
    ) -> Result<Self, StackDecodeError<E>> {
        if input.version != FORMAT_VERSION {
            return Err(StackDecodeError::version());
        }

        let values = input
            .routes
            .into_iter()
            .enumerate()
            .map(|(index, route)| {
                decode(route).map_err(|cause| StackDecodeError::route(index, cause))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut stack = Self::new();
        for value in values {
            stack.push(value);
        }

        Ok(stack)
    }
}
