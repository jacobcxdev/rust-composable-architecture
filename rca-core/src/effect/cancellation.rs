//! Erases typed cancellation identities without equality collisions.
//!
//! A key combines its concrete type, value and one runtime scope. Construction
//! hashes the type and value once. Clones share the owned value, and table hashes
//! use the cached fingerprint. Equal fingerprints still require concrete equality.
//! Keys must retain their `Hash` and `Eq` behaviour while the registry holds them.

use std::any::{Any, TypeId};
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::store::scope::ScopeToken;

/// Preserves concrete equality without requiring user keys to implement `Debug`.
trait ErasedCancellationValue: Send + Sync {
    /// Borrows the concrete key for a checked downcast or redacted type diagnostic.
    fn as_any(&self) -> &dyn Any;

    /// Compares keys only after their concrete types match.
    fn equals(&self, other: &dyn ErasedCancellationValue) -> bool;

    #[cfg(feature = "test-util")]
    /// Names the concrete key type without formatting its stored value.
    fn type_name(&self) -> &'static str;
}

/// Owns a typed key behind the registry's shared, erased identity storage.
struct CancellationValue<ID>(ID);

impl<ID> ErasedCancellationValue for CancellationValue<ID>
where
    ID: Eq + Send + Sync + 'static,
{
    fn as_any(&self) -> &dyn Any {
        &self.0
    }

    fn equals(&self, other: &dyn ErasedCancellationValue) -> bool {
        other.as_any().downcast_ref::<ID>() == Some(&self.0)
    }

    #[cfg(feature = "test-util")]
    fn type_name(&self) -> &'static str {
        std::any::type_name::<ID>()
    }
}

/// Selects typed cancellation generations within one structural runtime scope.
///
/// Cloning shares the key without calling its `Clone`, `Hash` or `Eq` methods.
/// Equality still calls user `Eq` after matching scope and cached hash.
#[derive(Clone)]
pub(crate) struct CancellationIdentity {
    /// Absent until effect compilation qualifies the key exactly once.
    scope: Option<ScopeToken>,
    /// Cached hash of the concrete type and key, not proof of equality.
    value_hash: u64,
    /// Keeps the original key alive for collision-safe equality and destruction.
    value: Arc<dyn ErasedCancellationValue>,
}

impl CancellationIdentity {
    /// Owns an unqualified key and caches its type-sensitive hash.
    ///
    /// User hashing runs before shared storage is allocated. A panic drops `id`
    /// without publishing an identity. No stable hash representation is promised.
    pub(crate) fn local<ID>(id: ID) -> Self
    where
        ID: Hash + Eq + Send + Sync + 'static,
    {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        TypeId::of::<ID>().hash(&mut hasher);
        Self {
            scope: None,
            value_hash: hasher.finish(),
            value: Arc::new(CancellationValue(id)),
        }
    }

    /// Qualifies an unscoped key without hashing or copying the user value.
    ///
    /// # Panics
    ///
    /// Panics if compilation qualifies an identity more than once.
    pub(crate) fn in_scope(mut self, scope: ScopeToken) -> Self {
        assert!(
            self.scope.replace(scope).is_none(),
            "cancellation identity cannot be qualified twice"
        );
        self
    }

    #[cfg(feature = "test-util")]
    /// Reads the compiled scope's opaque diagnostic identifier.
    ///
    /// # Panics
    ///
    /// Panics if the key has not passed through scope qualification.
    pub(crate) fn scope_id(&self) -> u64 {
        self.scope
            .expect("compiled cancellation identity must have a scope")
            .diagnostic_id()
    }

    #[cfg(feature = "test-util")]
    /// Names the concrete key type without exposing its value.
    pub(crate) fn type_name(&self) -> &'static str {
        self.value.type_name()
    }
}

impl PartialEq for CancellationIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope
            && self.value_hash == other.value_hash
            && self.value.equals(other.value.as_ref())
    }
}

impl Eq for CancellationIdentity {}

impl Hash for CancellationIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.scope.hash(state);
        self.value_hash.hash(state);
    }
}

impl fmt::Debug for CancellationIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationIdentity")
            .field("scope", &self.scope)
            .field("type_id", &self.value.as_any().type_id())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::hash::{Hash, Hasher};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::CancellationIdentity;
    use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot};

    #[derive(Eq, PartialEq)]
    struct Collision(u8);

    struct Counted {
        hashes: Arc<AtomicUsize>,
        equalities: Arc<AtomicUsize>,
    }

    impl PartialEq for Counted {
        fn eq(&self, _other: &Self) -> bool {
            self.equalities.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    impl Eq for Counted {}

    impl Hash for Counted {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.hashes.fetch_add(1, Ordering::Relaxed);
            7_u64.hash(state);
        }
    }

    #[derive(Default)]
    struct HashBytes(usize);

    impl Hasher for HashBytes {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, bytes: &[u8]) {
            self.0 += bytes.len();
        }
    }

    #[test]
    fn cached_identity_hash_preserves_user_callbacks() {
        let hashes = Arc::new(AtomicUsize::new(0));
        let equalities = Arc::new(AtomicUsize::new(0));
        let make = || CancellationIdentity::local(Counted {
            hashes: Arc::clone(&hashes),
            equalities: Arc::clone(&equalities),
        });
        let first = make();
        let second = make();
        let cloned = first.clone();
        assert_eq!(hashes.load(Ordering::Relaxed), 2);
        let mut bytes = HashBytes::default();
        for _ in 0..16 {
            first.hash(&mut bytes);
            assert_eq!(first, second);
            assert_eq!(first, cloned);
        }
        assert_eq!(hashes.load(Ordering::Relaxed), 2);
        assert_eq!(equalities.load(Ordering::Relaxed), 32);
        let hash = |identity: &CancellationIdentity| {
            let mut digest = std::collections::hash_map::DefaultHasher::new();
            identity.hash(&mut digest);
            digest.finish()
        };
        assert_eq!(hash(&first), hash(&second));
        eprintln!(
            "cancellation layout identity={} generation={} program={} effect={} declaration={}; outer_hash_bytes_for_16_hashes={}",
            std::mem::size_of::<CancellationIdentity>(),
            std::mem::size_of::<crate::effect::registry::GenerationToken>(),
            std::mem::size_of::<crate::effect::Program<()>>(),
            std::mem::size_of::<crate::Effect<()>>(),
            std::mem::size_of::<crate::effect::operation::OperationDeclaration>(),
            bytes.0,
        );
    }

    impl Hash for Collision {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0_u8.hash(state);
        }
    }

    #[test]
    fn identity_checks_concrete_values_after_hash_collision() {
        assert_ne!(
            CancellationIdentity::local(Collision(1)),
            CancellationIdentity::local(Collision(2))
        );
    }

    #[test]
    fn identity_keeps_concrete_types_distinct() {
        let first = CancellationIdentity::local(1_u8);
        let mut second = CancellationIdentity::local(1_u16);
        assert_ne!(first, second);
        // Force a cached fingerprint collision to exercise the concrete type check.
        second.value_hash = first.value_hash;
        assert_ne!(first, second);
    }

    #[test]
    fn colliding_registry_keys_cancel_only_the_equal_value() {
        let mut registry = crate::effect::registry::TaskRegistry::new(
            crate::effect::lineage::ControlWake::new(),
        );
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let scope = scopes.root();
        let selected = CancellationIdentity::local(Collision(1)).in_scope(scope);
        let sibling = CancellationIdentity::local(Collision(2)).in_scope(scope);
        assert_eq!(selected.value_hash, sibling.value_hash);
        let selected_generation = registry.register_generation(selected);
        let sibling_generation = registry.register_generation(sibling.clone());
        let equal_selected = CancellationIdentity::local(Collision(1)).in_scope(scope);

        assert_eq!(registry.cancel_identity(&equal_selected), [] as [u64; 0]);
        assert!(!selected_generation.is_live());
        assert!(sibling_generation.is_live());
        assert_eq!(registry.cancel_identity(&equal_selected), [] as [u64; 0]);
        assert!(sibling_generation.is_live());
        assert_eq!(registry.cancel_identity(&sibling), [] as [u64; 0]);
        assert!(!sibling_generation.is_live());
    }

    #[test]
    fn equal_typed_values_share_identity() {
        assert_eq!(
            CancellationIdentity::local(7_u64),
            CancellationIdentity::local(7_u64)
        );
    }

    #[test]
    fn diagnostics_redact_non_debug_values_without_changing_identity() {
        #[derive(Hash, PartialEq, Eq)]
        struct PrivateId(&'static str);

        let identity = CancellationIdentity::local(PrivateId("private-account-4821"));
        let diagnostic = format!("{identity:?}");
        assert!(diagnostic.starts_with("CancellationIdentity"));
        assert!(!diagnostic.contains("private-account-4821"));
        assert_eq!(
            identity,
            CancellationIdentity::local(PrivateId("private-account-4821"))
        );
        assert_ne!(
            identity,
            CancellationIdentity::local(PrivateId("private-account-9302"))
        );
    }

    #[test]
    fn equal_local_values_are_distinct_across_runtime_scopes() {
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root = scopes.root();
        let first_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let second_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let nested_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let first = scopes.mount(root, first_node, ScopeIdentity::unit());
        let second = scopes.mount(root, second_node, ScopeIdentity::unit());
        let nested = scopes.mount(first, nested_node, ScopeIdentity::unit());

        assert_ne!(
            CancellationIdentity::local(7_u8).in_scope(first),
            CancellationIdentity::local(7_u8).in_scope(second)
        );
        assert_ne!(
            CancellationIdentity::local(7_u8).in_scope(root),
            CancellationIdentity::local(7_u8).in_scope(first)
        );
        assert_ne!(
            CancellationIdentity::local(7_u8).in_scope(first),
            CancellationIdentity::local(7_u8).in_scope(nested)
        );
    }

    #[cfg(feature = "test-util")]
    #[test]
    fn identity_reports_type_without_storing_metadata() {
        assert_eq!(
            CancellationIdentity::local(7_u64).type_name(),
            std::any::type_name::<u64>()
        );
    }
}
}
