//! Defines store-owned dependency values.
//!
//! Each environment owns its overrides, default cache and registration history.
//! Reduction borrows that environment. Action and feature overlays borrow their
//! values for one synchronous operation instead of changing the store's cache.

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

mod application;
mod defaults;
mod fixture;
mod preparation;
#[doc(inline)]
pub use application::{ApplicationIds, CivilData, RandomBytes, WallTime};
use defaults::RegistrationRejection;
#[doc(inline)]
pub use defaults::{DependencyDefaultPolicy, DependencyDefaults, DependencyRegistrationError};
#[doc(inline)]
pub use fixture::{DependencyMockMismatch, try_dependency_mock};
#[doc(inline)]
pub use preparation::DependencyPreparationError;

use crate::MonotonicInstant;
use crate::store::scope::{
    ScopeIdentity, ScopeNodeId, ScopeRegistry, ScopeSlot, ScopeToken, ScopeTransitionBatch,
};

/// Owns one typed feature override for repeated, allocation-free reduction borrows.
pub(crate) struct SharedDependency {
    /// Identifies the key, rather than the possibly shared value type.
    key: TypeId,
    /// Stores exactly the value type associated with `key` at construction.
    value: Box<dyn Any + Send + Sync>,
}

impl SharedDependency {
    /// Erases one owned value while retaining its associated key identity.
    pub(crate) fn new<K: DependencyKey>(value: K::Value) -> Self {
        Self {
            key: TypeId::of::<K>(),
            value: Box::new(value),
        }
    }
}

/// Borrows one lexical override and the next outer override in the same tier.
struct DependencyLayer<'a> {
    /// Identifies the key whose associated value is borrowed by this layer.
    key: TypeId,
    /// Borrows the value without extending its owner's lifetime or cloning it.
    value: &'a (dyn Any + Send + Sync),
    /// Searches outer layers only when this layer has a different key.
    parent: Option<&'a DependencyLayer<'a>>,
}

impl DependencyLayer<'_> {
    /// Returns the innermost value for a key, or lets another tier supply it.
    fn value<K: DependencyKey>(&self) -> Option<&K::Value> {
        let mut layer = Some(self);
        while let Some(current) = layer {
            if current.key == TypeId::of::<K>() {
                return Some(
                    current
                        .value
                        .downcast_ref::<K::Value>()
                        .expect("dependency key must retain one value type"),
                );
            }
            layer = current.parent;
        }
        None
    }
}

/// Defines one typed dependency and its live, test and preview defaults.
pub trait DependencyKey: Send + Sync + 'static {
    /// Value stored for this key.
    type Value: Send + Sync + 'static;

    /// Creates this dependency's live value, when its interface owns a provider.
    ///
    /// Interface-only keys may omit this method and register owned constructors
    /// with [`DependencyValues::register_defaults`] instead.
    ///
    /// # Panics
    /// The default implementation names the missing dependency and panics.
    #[must_use]
    fn live_value() -> Self::Value {
        panic!(
            "missing live dependency `{}`; inject a value or register live defaults",
            std::any::type_name::<Self>()
        );
    }

    /// Creates this dependency's deterministic test default.
    ///
    /// `TestStore` calls this only when no explicit value applies. Override it for
    /// reusable deterministic defaults, or inject a value into each test store.
    /// Production stores do not call this method implicitly.
    ///
    /// # Panics
    ///
    /// The default implementation panics to prevent accidental live I/O in tests.
    #[must_use]
    fn test_value() -> Self::Value {
        panic!(
            "missing test dependency `{}`; inject a value or implement DependencyKey::test_value",
            std::any::type_name::<Self>()
        );
    }

    /// Creates this dependency's preview value, defaulting to its test value.
    ///
    /// Preview never falls back to the live default.
    ///
    /// # Panics
    /// Propagates the selected test default's panic when not overridden.
    #[must_use]
    fn preview_value() -> Self::Value {
        Self::test_value()
    }
}

/// Stores typed dependencies for one store.
///
/// Clones own independent maps and lookup history. Existing values and registered
/// constructors remain shared through reference counting. A clone made before a
/// default is first requested invokes the selected constructor for its own cache.
///
/// Values must be thread-safe even when the environment belongs to a local store.
/// No global or thread-local environment participates in lookup.
///
/// # Examples
///
/// ```
/// use rca_core::{DependencyKey, DependencyValues};
///
/// struct RetryLimit;
/// impl DependencyKey for RetryLimit {
///     type Value = usize;
///     fn live_value() -> usize { 3 }
/// }
///
/// let mut defaults = DependencyValues::new();
/// let mut customised = defaults.overriding::<RetryLimit>(5);
/// assert_eq!(*defaults.get_or_insert::<RetryLimit>(), 3);
/// assert_eq!(*customised.get_or_insert::<RetryLimit>(), 5);
/// ```
#[derive(Clone, Default)]
pub struct DependencyValues {
    /// Holds explicit store overrides, which mask cached and registered defaults.
    values: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    /// Holds successful factory results until replacement or an explicit reset.
    default_values: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    /// Holds each key's owned constructor bundle, erased as `DependencyDefaults<K>`.
    defaults: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
    /// Selects a constructor only when no explicit value or cached default applies.
    policy: DependencyDefaultPolicy,
    /// Permanently closes preparation after any direct environment lookup begins.
    used: bool,
    /// Closes registration for attempted defaults, including factories that panic.
    attempted_defaults: HashSet<TypeId>,
    /// Masks every reduction lookup tier with deterministic test values.
    #[cfg(feature = "test-util")]
    test_values: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl DependencyValues {
    /// Creates an empty dependency environment.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an empty environment with an explicitly selected default policy.
    #[must_use]
    pub fn for_policy(policy: DependencyDefaultPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// Prepares this owned environment before its first lookup.
    ///
    /// The callback may register defaults and set overrides without running factories.
    /// Repeated preparation is permitted until any `get_or_insert` lookup, including
    /// an overridden lookup or a failed factory. Lookup within the callback also ends
    /// preparation eligibility. Overrides remain mutable afterwards; registration
    /// freezes per key only when its default is attempted. This is not transactional:
    /// callback results and partial mutations are retained, including during unwinding.
    ///
    /// # Errors
    /// Returns an error without invoking the callback after this environment was used.
    ///
    /// # Panics
    /// Propagates callback panics.
    pub fn prepare<R>(
        &mut self,
        update: impl FnOnce(&mut Self) -> R,
    ) -> Result<R, DependencyPreparationError> {
        if self.used {
            return Err(DependencyPreparationError::new());
        }
        Ok(update(self))
    }

    /// Clears only this environment's cached defaults.
    ///
    /// Registrations, explicit overrides, policy and use/registration history remain.
    /// The next unoverridden lookup invokes its selected factory again. Clones own
    /// independent cache maps but share already-created Arc values and constructors;
    /// cloning before first lookup can therefore invoke a factory once per clone.
    /// Reset does not revoke existing handles, clear external storage, guarantee a
    /// fresh endpoint or wait for backend work to stop. Dropping the final cached
    /// owner may run the value's destructor.
    ///
    /// # Panics
    /// Propagates cached value destructor panics.
    pub fn reset_cache(&mut self) {
        self.default_values.clear();
    }

    /// Registers owned lazy defaults without replacing existing registrations or caches.
    ///
    /// Explicit overrides, including `None`, retain precedence and do not prevent
    /// registration. Register before the first default attempt for this key. No
    /// constructors run here; missing constructors retain the key's fail-closed
    /// default behaviour.
    ///
    /// # Errors
    /// Returns the submitted bundle when the key is already registered or its
    /// default has already been attempted, including a failed factory. Reset and
    /// explicit replacement do not reopen registration. Other keys remain eligible.
    /// Existing environment state is unchanged.
    pub fn register_defaults<K: DependencyKey>(
        &mut self,
        defaults: DependencyDefaults<K>,
    ) -> Result<(), DependencyRegistrationError<K>> {
        let key = TypeId::of::<K>();
        let rejection = if self.defaults.contains_key(&key) {
            Some(RegistrationRejection::Duplicate)
        } else if self.attempted_defaults.contains(&key) {
            Some(RegistrationRejection::Initialized)
        } else {
            None
        };
        if let Some(reason) = rejection {
            return Err(DependencyRegistrationError::new(defaults, reason));
        }
        self.defaults.insert(key, Arc::new(defaults));
        Ok(())
    }

    /// Sets a dependency value.
    ///
    /// Removes this key's cached default without changing its registration or lookup
    /// history. Other environment clones retain their own values and cache entries.
    /// Replacement is not transactional: cached values are dropped before insertion.
    ///
    /// # Panics
    /// Propagates value destructor panics when replacement releases the final owner.
    pub fn set<K: DependencyKey>(&mut self, value: K::Value) {
        let key = TypeId::of::<K>();
        self.default_values.remove(&key);
        self.values.insert(key, Arc::new(value));
    }

    /// Sets a highest-priority dependency for deterministic test stores.
    #[cfg(feature = "test-util")]
    pub(crate) fn set_test<K: DependencyKey>(&mut self, value: K::Value) {
        self.test_values.insert(TypeId::of::<K>(), Arc::new(value));
    }

    /// Selects test defaults, discarding cached values only when the policy changes.
    ///
    /// Explicit overrides, registrations and lookup history remain unchanged.
    #[cfg(feature = "test-util")]
    pub(crate) fn use_test_defaults(&mut self) {
        if self.policy != DependencyDefaultPolicy::Test {
            self.default_values.clear();
            self.policy = DependencyDefaultPolicy::Test;
        }
    }

    /// Clones this environment and overrides one value.
    #[must_use]
    pub fn overriding<K: DependencyKey>(&self, value: K::Value) -> Self {
        let mut overridden = self.clone();
        overridden.set::<K>(value);
        overridden
    }

    /// Returns a value, creating the environment's default once when absent.
    ///
    /// New environments use live defaults; [`Self::for_policy`] selects a policy.
    /// `TestStore` selects test defaults after importing an environment, discarding
    /// cached defaults from another policy but retaining explicit overrides.
    /// Cloning an environment shares already-created values and constructor handles.
    /// Failed factories leave no cached value, so later lookups retry them. Both
    /// preparation and registration for that key remain closed after a failed attempt.
    /// [`Self::reset_cache`] discards successful defaults without clearing that history.
    ///
    /// # Panics
    ///
    /// Panics if the selected default factory panics or RCA's typed-key invariant is violated.
    pub fn get_or_insert<K: DependencyKey>(&mut self) -> &K::Value {
        self.used = true;
        if let Some(value) = self.values.get(&TypeId::of::<K>()) {
            return value
                .downcast_ref::<K::Value>()
                .expect("dependency key must retain one value type");
        }
        let policy = self.policy;
        let defaults = &self.defaults;
        let attempted_defaults = &mut self.attempted_defaults;
        self.default_values
            .entry(TypeId::of::<K>())
            .or_insert_with(|| {
                attempted_defaults.insert(TypeId::of::<K>());
                let value = if let Some(defaults) = defaults.get(&TypeId::of::<K>()) {
                    defaults
                        .downcast_ref::<DependencyDefaults<K>>()
                        .expect("dependency key must retain one default bundle type")
                        .create(policy)
                } else {
                    match policy {
                        DependencyDefaultPolicy::Live => K::live_value(),
                        DependencyDefaultPolicy::Test => K::test_value(),
                        DependencyDefaultPolicy::Preview => K::preview_value(),
                    }
                };
                Arc::new(value)
            })
            .downcast_ref::<K::Value>()
            .expect("dependency key must retain one value type")
    }
}

impl fmt::Debug for DependencyValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("DependencyValues");
        debug.field("len", &self.values.len());
        debug.field("default_len", &self.default_values.len());
        debug.field("defaults_len", &self.defaults.len());
        debug.field("policy", &self.policy);
        debug.field("used", &self.used);
        debug.field("attempted_defaults_len", &self.attempted_defaults.len());
        #[cfg(feature = "test-util")]
        debug.field("test_len", &self.test_values.len());
        debug.finish()
    }
}

/// Lends store-owned dependencies during reduction.
///
/// Lookup precedence is deterministic test values, action overrides, feature
/// overrides, explicit store values, then the store's selected defaults.
/// Within each overlay tier, the innermost matching key wins.
///
/// This context cannot outlive reduction. For asynchronous work, capture an owned
/// dependency handle or the result of [`Self::issue_reporter`] or [`Self::dismiss`].
pub struct ReduceContext<'a> {
    /// Keeps the store's reporting endpoint available to borrowed child contexts.
    issues: crate::IssueReporter,
    /// Names the nearest presentation lifetime, without extending its authority.
    dismiss_target: Option<crate::composition::DismissTarget>,
    /// Borrows the store's cache and overrides exclusively for this reduction.
    dependencies: &'a mut DependencyValues,
    /// Captures one monotonic time for the entire reduction and its child contexts.
    now: MonotonicInstant,
    /// Borrows the registry of live scopes that this reduction can enter or retire.
    scopes: &'a mut ScopeRegistry,
    /// Selects the parent for newly mounted scopes and input gate operations.
    current_scope: ScopeToken,
    /// Collects retirements for the store's later effect and task reconciliation.
    transitions: &'a mut ScopeTransitionBatch,
    /// Borrows reducer-owned overlays, from the innermost layer outwards.
    feature_dependencies: Option<&'a DependencyLayer<'a>>,
    /// Borrows action-specific overlays, independently of feature nesting.
    explicit_dependencies: Option<&'a DependencyLayer<'a>>,
    /// Shares this reduction's lifecycle diagnostic sequence with nested operations.
    #[cfg(feature = "diagnostics")]
    lifecycle: Option<crate::diagnostics::LifecycleCall<'a>>,
}

impl<'a> ReduceContext<'a> {
    /// Starts a root context without lexical overlays or presentation authority.
    pub(crate) fn new(
        dependencies: &'a mut DependencyValues,
        now: MonotonicInstant,
        scopes: &'a mut ScopeRegistry,
        transitions: &'a mut ScopeTransitionBatch,
    ) -> Self {
        Self {
            issues: crate::IssueReporter::default(),
            dismiss_target: None,
            dependencies,
            now,
            current_scope: scopes.root(),
            scopes,
            transitions,
            feature_dependencies: None,
            explicit_dependencies: None,
            #[cfg(feature = "diagnostics")]
            lifecycle: None,
        }
    }

    /// Replaces the endpoint inherited by child contexts and captured reporters.
    pub(crate) fn with_issue_reporter(mut self, issues: crate::IssueReporter) -> Self {
        self.issues = issues;
        self
    }

    /// Attaches the store's current lifecycle call without starting a new sequence.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn with_lifecycle_diagnostics(
        mut self,
        call: Option<crate::diagnostics::LifecycleCall<'a>>,
    ) -> Self {
        self.lifecycle = call;
        self
    }

    /// Lends all context state for a shorter borrow, retaining the active scope.
    pub(crate) fn reborrow(&mut self) -> ReduceContext<'_> {
        ReduceContext {
            issues: self.issues.clone(),
            dismiss_target: self.dismiss_target.clone(),
            dependencies: self.dependencies,
            now: self.now,
            scopes: self.scopes,
            current_scope: self.current_scope,
            transitions: self.transitions,
            feature_dependencies: self.feature_dependencies,
            explicit_dependencies: self.explicit_dependencies,
            #[cfg(feature = "diagnostics")]
            lifecycle: self
                .lifecycle
                .as_mut()
                .map(crate::diagnostics::LifecycleCall::reborrow),
        }
    }

    /// Exposes scope liveness without permitting registry changes.
    pub(crate) fn scopes(&self) -> &ScopeRegistry {
        self.scopes
    }

    /// Returns this Store's reporter for synchronous or captured asynchronous work.
    #[must_use]
    pub fn issue_reporter(&self) -> crate::IssueReporter {
        self.issues.clone()
    }

    /// Captures owned dismissal authority for the nearest presentation lifetime.
    ///
    /// Cloning or retaining the returned client does not renew its authority.
    /// Outside presentation reduction, calling [`crate::Dismiss::effect`] reports
    /// a required issue; requesting the client alone does not report.
    #[must_use]
    pub fn dismiss(&self) -> crate::Dismiss {
        crate::Dismiss::new(self.dismiss_target.clone(), self.issues.clone())
    }

    /// Runs child reduction with temporary authority for its presentation lifetime.
    ///
    /// The parent retains its dismissal target, including when the operation panics.
    pub(crate) fn with_dismiss_target<Output>(
        &mut self,
        target: crate::composition::DismissTarget,
        operation: impl FnOnce(&mut ReduceContext<'_>) -> Output,
    ) -> Output {
        let mut child = ReduceContext {
            issues: self.issues.clone(),
            dismiss_target: Some(target),
            dependencies: self.dependencies,
            now: self.now,
            scopes: self.scopes,
            current_scope: self.current_scope,
            transitions: self.transitions,
            feature_dependencies: self.feature_dependencies,
            explicit_dependencies: self.explicit_dependencies,
            #[cfg(feature = "diagnostics")]
            lifecycle: self
                .lifecycle
                .as_mut()
                .map(crate::diagnostics::LifecycleCall::reborrow),
        };
        operation(&mut child)
    }

    /// Reports a recoverable issue through this Store's endpoint.
    ///
    /// # Errors
    /// Returns the original issue when reporting is disabled or closed.
    ///
    /// # Panics
    /// Propagates any configured issue handler panic.
    pub fn report_issue(&self, issue: crate::Issue) -> Result<(), crate::IssueReportError> {
        self.issues.report(issue)
    }

    /// Returns a dependency, lazily creating the Store's selected default when absent.
    ///
    /// Ordinary Stores default to Live but honour an imported environment's policy.
    /// `TestStore` always selects Test. Explicit test, action, feature and Store
    /// values take precedence over defaults.
    ///
    /// # Panics
    ///
    /// Panics if the selected default factory panics (including an unimplemented
    /// test default) or RCA's internal typed-key invariant is violated.
    pub fn dependency<K: DependencyKey>(&mut self) -> &K::Value {
        #[cfg(feature = "test-util")]
        if self
            .dependencies
            .test_values
            .contains_key(&TypeId::of::<K>())
        {
            // Borrow only after the presence check: returning a single `get` borrow
            // prevents the later mutable fallback on the supported compiler.
            let value = self
                .dependencies
                .test_values
                .get(&TypeId::of::<K>())
                .expect("dependency was present")
                .downcast_ref::<K::Value>()
                .expect("dependency key must retain one value type");
            return value;
        }
        if let Some(value) = self
            .explicit_dependencies
            .and_then(DependencyLayer::value::<K>)
        {
            return value;
        }
        if let Some(value) = self
            .feature_dependencies
            .and_then(DependencyLayer::value::<K>)
        {
            return value;
        }
        self.dependencies.get_or_insert::<K>()
    }

    /// Borrows a reducer-owned override without changing store values or caches.
    pub(crate) fn with_feature_dependency<Output>(
        &mut self,
        dependency: &SharedDependency,
        operation: impl FnOnce(&mut ReduceContext<'_>) -> Output,
    ) -> Output {
        let layer = DependencyLayer {
            key: dependency.key,
            value: dependency.value.as_ref(),
            parent: self.feature_dependencies,
        };
        let mut child = ReduceContext {
            issues: self.issues.clone(),
            dismiss_target: self.dismiss_target.clone(),
            dependencies: self.dependencies,
            now: self.now,
            scopes: self.scopes,
            current_scope: self.current_scope,
            transitions: self.transitions,
            feature_dependencies: Some(&layer),
            explicit_dependencies: self.explicit_dependencies,
            #[cfg(feature = "diagnostics")]
            lifecycle: self
                .lifecycle
                .as_mut()
                .map(crate::diagnostics::LifecycleCall::reborrow),
        };
        operation(&mut child)
    }

    /// Runs one synchronous operation beneath an action-specific dependency override.
    ///
    /// This override has higher precedence than reducer feature overlays. Its borrowed
    /// environment cannot escape `operation`. Deterministic test values still win.
    /// The override is dropped after the operation, including during unwinding.
    /// Parent dependencies remain unchanged. Other context changes are not rolled back.
    ///
    /// # Panics
    /// Propagates operation panics and the override value's destructor panic.
    pub fn with_dependency_override<K, Output>(
        &mut self,
        value: K::Value,
        operation: impl FnOnce(&mut ReduceContext<'_>) -> Output,
    ) -> Output
    where
        K: DependencyKey,
    {
        let output = {
            let layer = DependencyLayer {
                key: TypeId::of::<K>(),
                value: &value,
                parent: self.explicit_dependencies,
            };
            let mut child = ReduceContext {
                issues: self.issues.clone(),
                dismiss_target: self.dismiss_target.clone(),
                dependencies: self.dependencies,
                now: self.now,
                scopes: self.scopes,
                current_scope: self.current_scope,
                transitions: self.transitions,
                feature_dependencies: self.feature_dependencies,
                explicit_dependencies: Some(&layer),
                #[cfg(feature = "diagnostics")]
                lifecycle: self
                    .lifecycle
                    .as_mut()
                    .map(crate::diagnostics::LifecycleCall::reborrow),
            };
            operation(&mut child)
        };
        drop(value);
        output
    }

    /// Returns the monotonic time captured at the start of this reduction.
    #[must_use]
    pub const fn now(&self) -> MonotonicInstant {
        self.now
    }

    /// Resolves a structural slot to its stable node identity.
    pub(crate) fn node(slot: &mut ScopeSlot) -> ScopeNodeId {
        ScopeRegistry::node(slot)
    }

    /// Shares the store-local namespace used to bind dismissal authority.
    pub(crate) fn presentation_namespace(&mut self) -> Arc<()> {
        self.scopes.presentation_namespace()
    }

    /// Changes a gate only when it belongs to this context's active lifetime.
    pub(crate) fn set_input_open(
        &mut self,
        gate: &crate::InputGate,
        open: bool,
    ) -> Result<(), crate::TaskAccessError> {
        self.scopes.set_input_open(self.current_scope, gate, open)
    }

    #[cfg(test)]
    ordinary_test_items! {
        pub(crate) const fn scope_token(&self) -> ScopeToken {
            self.current_scope
        }
    }

    /// Borrows typed reconciliation state from a scope, without creating it.
    pub(crate) fn scope_cache<T: Send + 'static>(&self, token: ScopeToken) -> Option<&T> {
        self.scopes.scope_cache(token)
    }

    /// Removes a scope's typed reconciliation state for an owned update.
    pub(crate) fn take_scope_cache<T: Send + 'static>(
        &mut self,
        token: ScopeToken,
    ) -> Option<Box<T>> {
        self.scopes.take_scope_cache(token)
    }

    /// Returns owned reconciliation state to its live scope.
    pub(crate) fn put_scope_cache<T: Send + 'static>(&mut self, token: ScopeToken, cache: Box<T>) {
        self.scopes.put_scope_cache(token, cache);
    }

    /// Resolves a child lifetime and reports installation only when it is new.
    pub(crate) fn mount<Value>(
        &mut self,
        node: ScopeNodeId,
        identity: ScopeIdentity<Value>,
    ) -> ScopeToken
    where
        Value: Eq + Hash + Send + Sync + 'static,
    {
        let outcome = self
            .scopes
            .mount_outcome(self.current_scope, node, identity);
        #[cfg(feature = "diagnostics")]
        if outcome.inserted
            && let Some(call) = self.lifecycle.as_mut()
        {
            call.emit(crate::diagnostics::LifecycleKind::Installed {
                lifetime: outcome.token.diagnostic_id(),
                parent: Some(self.current_scope.diagnostic_id()),
                configuration: None,
            });
        }
        outcome.token
    }

    /// Selects a token already validated by the transaction boundary before user entry.
    pub(crate) fn in_scope(mut self, token: ScopeToken) -> Self {
        self.current_scope = token;
        self
    }

    /// Reports whether the registry still permits installation during this transaction.
    pub(crate) fn installation_is_live(&self) -> bool {
        self.scopes.installation_is_live()
    }

    /// Enters a live scope and restores the previous selection, including during unwinding.
    ///
    /// Scope mutations remain in the registry. Restoring selection does not restore
    /// a retired lifetime or roll back the operation's other changes.
    ///
    /// # Panics
    /// Rejects an inactive token before the callback and propagates callback panics.
    pub(crate) fn with_scope<T>(
        &mut self,
        token: ScopeToken,
        operation: impl FnOnce(&mut Self) -> T,
    ) -> T {
        assert!(
            self.scopes.is_current(token),
            "cannot reduce within an inactive scope"
        );
        let previous = std::mem::replace(&mut self.current_scope, token);
        let guard = ScopeGuard {
            context: self,
            previous,
        };
        operation(&mut *guard.context)
    }

    /// Retires a scope subtree and records transitions before lifecycle notifications.
    pub(crate) fn leave(&mut self, token: ScopeToken) {
        let retired = self.scopes.leave(token);
        #[cfg(feature = "diagnostics")]
        {
            self.transitions.extend(retired.iter().copied());
            if let Some(call) = self.lifecycle.as_mut() {
                for token in retired {
                    call.emit(crate::diagnostics::LifecycleKind::Retired {
                        lifetime: token.diagnostic_id(),
                    });
                }
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        self.transitions.extend(retired);
    }
}

/// Restores the context's selected scope without undoing registry mutations.
struct ScopeGuard<'context, 'store> {
    /// Exclusively borrows the context while its temporary scope is selected.
    context: &'context mut ReduceContext<'store>,
    /// Remembers the selection to restore on normal return or unwinding.
    previous: ScopeToken,
}

impl Drop for ScopeGuard<'_, '_> {
    fn drop(&mut self) {
        self.context.current_scope = self.previous;
    }
}

impl fmt::Debug for ReduceContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReduceContext")
            .field("now", &self.now)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    use super::{DependencyKey, DependencyValues, ReduceContext};
    use crate::MonotonicInstant;
    use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot};

    #[test]
    fn nested_scope_panics_restore_selection_without_undoing_retirement() {
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root_token = scopes.root();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let outer = context.mount(
            ReduceContext::node(&mut ScopeSlot::new()),
            ScopeIdentity::unit(),
        );
        let mut retired_inner = None;

        let panic = catch_unwind(AssertUnwindSafe(|| {
            context.with_scope(outer, |context| {
                assert_eq!(context.current_scope, outer);
                let inner = context.mount(
                    ReduceContext::node(&mut ScopeSlot::new()),
                    ScopeIdentity::unit(),
                );
                let panic = catch_unwind(AssertUnwindSafe(|| {
                    context.with_scope(inner, |context| {
                        assert_eq!(context.current_scope, inner);
                        context.leave(inner);
                        panic!("inner scope panic");
                    });
                }))
                .expect_err("inner callback panic must propagate");
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"inner scope panic"));
                assert_eq!(context.current_scope, outer);
                assert!(!context.scopes().is_current(inner));
                retired_inner = Some(inner);
                panic!("outer scope panic");
            });
        }))
        .expect_err("outer callback panic must propagate");

        assert_eq!(panic.downcast_ref::<&str>(), Some(&"outer scope panic"));
        assert_eq!(context.current_scope, root_token);
        assert!(context.scopes().is_current(outer));
        assert!(context.scopes().is_current(root_token));
        drop(context);
        assert!(transitions.contains(retired_inner.expect("inner callback ran")));
        assert!(!transitions.contains(outer));
    }

    #[test]
    fn inactive_scope_rejects_the_callback_without_changing_selection() {
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root_token = scopes.root();
        let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let retired = context.mount(
            ReduceContext::node(&mut ScopeSlot::new()),
            ScopeIdentity::unit(),
        );
        context.leave(retired);
        let mut callback_ran = false;
        let panic = catch_unwind(AssertUnwindSafe(|| {
            context.with_scope(retired, |_| callback_ran = true);
        }))
        .expect_err("retired scope must be rejected");

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"cannot reduce within an inactive scope"),
        );
        assert!(!callback_ran);
        assert_eq!(context.current_scope, root_token);
        assert!(context.scopes().is_current(root_token));
    }

    #[test]
    fn dependency_override_preserves_root_transitions_and_releases_resources() {
        struct Resource;
        impl DependencyKey for Resource {
            type Value = Arc<()>;
        }

        let stored = Arc::new(());
        let stored_weak = Arc::downgrade(&stored);
        let override_value = Arc::new(());
        let override_weak = Arc::downgrade(&override_value);
        let mut dependencies = DependencyValues::new();
        dependencies.set::<Resource>(stored);
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root_token = scopes.root();
        let mut root_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut root = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut root_transitions,
        );
        let first = root.mount(
            ReduceContext::node(&mut ScopeSlot::new()),
            ScopeIdentity::unit(),
        );
        let second = root.mount(
            ReduceContext::node(&mut ScopeSlot::new()),
            ScopeIdentity::unit(),
        );
        root.leave(first);

        root.with_dependency_override::<Resource, _>(override_value, |child| {
            assert_eq!(Arc::strong_count(child.dependency::<Resource>()), 1);
            child.leave(second);
        });

        assert!(override_weak.upgrade().is_none());
        assert_eq!(root.current_scope, root_token);
        assert!(Arc::ptr_eq(
            root.dependency::<Resource>(),
            &stored_weak.upgrade().unwrap(),
        ));
        drop(root);
        let transitions = root_transitions;
        assert!(transitions.contains(first));
        assert!(transitions.contains(second));
        assert!(!transitions.contains(root_token));
        assert!(scopes.is_current(root_token));
        assert!(!scopes.is_current(first));
        assert!(!scopes.is_current(second));
        drop(transitions);
        drop(scopes);
        drop(dependencies);
        assert!(stored_weak.upgrade().is_none());
    }
}
}
