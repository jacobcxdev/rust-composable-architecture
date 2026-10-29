//! Defines owned, lazy dependency defaults and explicit environment policy.

use std::backtrace::Backtrace;
use std::fmt;
use std::sync::Arc;

use super::DependencyKey;

/// Selects defaults for one explicitly owned dependency environment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum DependencyDefaultPolicy {
    /// Uses registered live defaults or the key's live default.
    #[default]
    Live,
    /// Uses registered test defaults or the key's fail-closed test default.
    Test,
    /// Uses preview defaults, falling back to test defaults, never live defaults.
    Preview,
}

/// Shares a lazy constructor without requiring the key or its values to be cloneable.
type DefaultValue<K> = Arc<dyn Fn() -> <K as DependencyKey>::Value + Send + Sync>;

/// Owns optional lazy default constructors for one dependency key.
///
/// Register a bundle in an owned [`super::DependencyValues`] environment. This
/// allows an interface crate to declare a key without depending on its providers.
/// Constructors run on unoverridden lookups without a cached default. Cache reset
/// or a previous constructor panic permits another attempt. Each supplied closure
/// is heap-allocated; cloning shares closures, not newly created values.
/// Missing live/test constructors use the key's defaults. Missing preview uses
/// this bundle's test constructor, if present, otherwise the key's preview default.
/// Nothing discovers a provider or selects a policy globally.
///
/// # Examples
///
/// ```
/// use rca_core::{DependencyKey, DependencyValues};
/// use rca_core::{DependencyDefaultPolicy, DependencyDefaults};
/// struct Name;
/// impl DependencyKey for Name { type Value = String; }
/// let mut values = DependencyValues::for_policy(DependencyDefaultPolicy::Test);
/// values.register_defaults(DependencyDefaults::<Name>::new()
///     .with_test(|| "fixture".to_owned())).unwrap();
/// assert_eq!(values.get_or_insert::<Name>(), "fixture");
/// ```
///
/// A provider cannot register a constructor with an incompatible result type:
///
/// ```compile_fail,E0308
/// use rca_core::DependencyKey;
/// use rca_core::DependencyDefaults;
/// struct Count;
/// impl DependencyKey for Count { type Value = usize; }
/// let _ = DependencyDefaults::<Count>::new().with_test(|| "wrong type");
/// ```
pub struct DependencyDefaults<K: DependencyKey> {
    /// Overrides the key's live constructor without changing other policies.
    live: Option<DefaultValue<K>>,
    /// Supplies test construction and the bundle's preview fallback.
    test: Option<DefaultValue<K>>,
    /// Overrides both registered test and key-level preview construction.
    preview: Option<DefaultValue<K>>,
}

impl<K: DependencyKey> DependencyDefaults<K> {
    /// Creates a bundle using the key's defaults until constructors are supplied.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            live: None,
            test: None,
            preview: None,
        }
    }

    /// Supplies this bundle's lazy live constructor.
    #[must_use]
    pub fn with_live(mut self, make: impl Fn() -> K::Value + Send + Sync + 'static) -> Self {
        self.live = Some(Arc::new(make));
        self
    }

    /// Supplies this bundle's lazy deterministic test constructor.
    #[must_use]
    pub fn with_test(mut self, make: impl Fn() -> K::Value + Send + Sync + 'static) -> Self {
        self.test = Some(Arc::new(make));
        self
    }

    /// Supplies this bundle's lazy preview constructor.
    #[must_use]
    pub fn with_preview(mut self, make: impl Fn() -> K::Value + Send + Sync + 'static) -> Self {
        self.preview = Some(Arc::new(make));
        self
    }

    /// Runs the selected constructor; the owning environment handles caching and attempt history.
    pub(super) fn create(&self, policy: DependencyDefaultPolicy) -> K::Value {
        match policy {
            DependencyDefaultPolicy::Live => {
                self.live.as_ref().map_or_else(K::live_value, |make| make())
            }
            DependencyDefaultPolicy::Test => {
                self.test.as_ref().map_or_else(K::test_value, |make| make())
            }
            DependencyDefaultPolicy::Preview => self
                .preview
                .as_ref()
                .or(self.test.as_ref())
                .map_or_else(K::preview_value, |make| make()),
        }
    }
}

impl<K: DependencyKey> Default for DependencyDefaults<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: DependencyKey> Clone for DependencyDefaults<K> {
    fn clone(&self) -> Self {
        Self {
            live: self.live.clone(),
            test: self.test.clone(),
            preview: self.preview.clone(),
        }
    }
}

impl<K: DependencyKey> fmt::Debug for DependencyDefaults<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DependencyDefaults")
            .field("key", &std::any::type_name::<K>())
            .field("live", &self.live.is_some())
            .field("test", &self.test.is_some())
            .field("preview", &self.preview.is_some())
            .finish()
    }
}

/// Identifies which existing environment state prevents registration.
#[derive(Clone, Copy, Debug)]
pub(super) enum RegistrationRejection {
    /// A bundle already owns construction for this key.
    Duplicate,
    /// A default was attempted, even if construction failed or its cache was reset.
    Initialized,
}

/// Recovers a rejected default bundle without replacing existing environment state.
pub struct DependencyRegistrationError<K: DependencyKey> {
    /// Retains the submitted constructors so callers can recover them unchanged.
    defaults: DependencyDefaults<K>,
    /// Explains rejection without exposing the private admission enum.
    reason: RegistrationRejection,
    /// Captures the failed registration site when backtraces are enabled.
    backtrace: Backtrace,
}

impl<K: DependencyKey> DependencyRegistrationError<K> {
    /// Takes ownership of a rejected bundle without invoking its constructors.
    pub(super) fn new(defaults: DependencyDefaults<K>, reason: RegistrationRejection) -> Self {
        Self {
            defaults,
            reason,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether an existing bundle caused this rejection.
    #[must_use]
    pub const fn is_duplicate(&self) -> bool {
        matches!(self.reason, RegistrationRejection::Duplicate)
    }

    /// Returns whether a previous default attempt caused this rejection.
    ///
    /// Includes failed factories and survives cache reset or explicit replacement.
    /// An existing bundle takes precedence and reports [`Self::is_duplicate`] instead.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        matches!(self.reason, RegistrationRejection::Initialized)
    }

    /// Returns the rejected key's Rust type name.
    #[must_use]
    pub fn key_name(&self) -> &'static str {
        std::any::type_name::<K>()
    }

    /// Returns the backtrace captured when registration failed.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    /// Returns the submitted bundle for registration in another environment.
    #[must_use]
    pub fn into_defaults(self) -> DependencyDefaults<K> {
        self.defaults
    }
}

impl<K: DependencyKey> fmt::Debug for DependencyRegistrationError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DependencyRegistrationError")
            .field("defaults", &self.defaults)
            .field("reason", &self.reason)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl<K: DependencyKey> fmt::Display for DependencyRegistrationError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self.reason {
            RegistrationRejection::Duplicate => "defaults already registered",
            RegistrationRejection::Initialized => "default already attempted",
        };
        write!(
            f,
            "dependency `{}`: {reason}\n{}",
            self.key_name(),
            self.backtrace
        )
    }
}

impl<K: DependencyKey> std::error::Error for DependencyRegistrationError<K> {}
