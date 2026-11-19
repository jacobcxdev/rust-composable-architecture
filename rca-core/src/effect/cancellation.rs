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

/// Owns a cached key before the actor supplies its structural scope.
#[derive(Clone)]
pub(crate) struct UnscopedCancellationIdentity {
    /// Cached hash of the concrete type and key, not proof of equality.
    value_hash: u64,
    /// Keeps the original key alive for collision-safe equality and destruction.
    value: Arc<dyn ErasedCancellationValue>,
}

impl UnscopedCancellationIdentity {
    /// Owns a key and caches its type-sensitive hash before allocation.
    ///
    /// User hashing runs before shared storage is allocated. A panic drops `id`
    /// without publishing an identity. No stable hash representation is promised.
    pub(crate) fn new<ID>(id: ID) -> Self
    where
        ID: Hash + Eq + Send + Sync + 'static,
    {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        TypeId::of::<ID>().hash(&mut hasher);
        Self {
            value_hash: hasher.finish(),
            value: Arc::new(CancellationValue(id)),
        }
    }

    /// Transfers the same key into the actor's selected scope without rehashing.
    pub(crate) fn in_scope(self, scope: ScopeToken) -> CancellationIdentity {
        CancellationIdentity { scope, key: self }
    }
}

/// Selects typed cancellation generations within one structural runtime scope.
///
/// Cloning shares the key without calling its `Clone`, `Hash` or `Eq` methods.
/// Equality still calls user `Eq` after matching scope and cached hash.
#[derive(Clone)]
pub(crate) struct CancellationIdentity {
    /// Selects the runtime registry scope before lookup or registration.
    scope: ScopeToken,
    /// Retains the cached fingerprint and its concrete equality owner.
    key: UnscopedCancellationIdentity,
}

impl CancellationIdentity {
    #[cfg(feature = "test-util")]
    /// Reads the compiled scope's opaque diagnostic identifier.
    pub(crate) fn scope_id(&self) -> u64 {
        self.scope.diagnostic_id()
    }

    #[cfg(feature = "test-util")]
    /// Names the concrete key type without exposing its value.
    pub(crate) fn type_name(&self) -> &'static str {
        self.key.value.type_name()
    }
}

impl PartialEq for CancellationIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope
            && self.key.value_hash == other.key.value_hash
            && self.key.value.equals(other.key.value.as_ref())
    }
}

impl Eq for CancellationIdentity {}

impl Hash for CancellationIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.scope.hash(state);
        self.key.value_hash.hash(state);
    }
}

impl fmt::Debug for CancellationIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationIdentity")
            .field("scope", &Some(self.scope))
            .field("type_id", &self.key.value.as_any().type_id())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::hash::{Hash, Hasher};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{CancellationIdentity, UnscopedCancellationIdentity};
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

    #[test]
    fn unscoped_keys_qualify_without_rehashing_or_replacing_ownership() {
        let hashes = Arc::new(AtomicUsize::new(0));
        let equalities = Arc::new(AtomicUsize::new(0));
        let key = UnscopedCancellationIdentity::new(Counted {
            hashes: Arc::clone(&hashes),
            equalities: Arc::clone(&equalities),
        });
        let cloned = key.clone();
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let qualified = key.in_scope(scopes.root());

        assert_eq!(hashes.load(Ordering::Relaxed), 1);
        assert!(Arc::ptr_eq(&cloned.value, &qualified.key.value));
        assert_eq!(qualified, cloned.in_scope(scopes.root()));
        assert_eq!(hashes.load(Ordering::Relaxed), 1);
        assert_eq!(equalities.load(Ordering::Relaxed), 1);
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
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let make = || UnscopedCancellationIdentity::new(Counted {
            hashes: Arc::clone(&hashes),
            equalities: Arc::clone(&equalities),
        }).in_scope(scopes.root());
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
        assert_eq!(bytes.0, 16 * 2 * std::mem::size_of::<u64>());
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
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        assert_ne!(
            UnscopedCancellationIdentity::new(Collision(1)).in_scope(scopes.root()),
            UnscopedCancellationIdentity::new(Collision(2)).in_scope(scopes.root())
        );
    }

    #[test]
    fn identity_keeps_concrete_types_distinct() {
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let first = UnscopedCancellationIdentity::new(1_u8).in_scope(scopes.root());
        let mut second = UnscopedCancellationIdentity::new(1_u16).in_scope(scopes.root());
        assert_ne!(first, second);
        // Force a cached fingerprint collision to exercise the concrete type check.
        second.key.value_hash = first.key.value_hash;
        assert_ne!(first, second);
    }

    #[test]
    fn colliding_registry_keys_cancel_only_the_equal_value() {
        let mut registry = crate::effect::registry::TaskRegistry::new(
            crate::effect::lineage::ControlWake::new(),
        );
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let scope = scopes.root();
        let selected = UnscopedCancellationIdentity::new(Collision(1)).in_scope(scope);
        let sibling = UnscopedCancellationIdentity::new(Collision(2)).in_scope(scope);
        assert_eq!(selected.key.value_hash, sibling.key.value_hash);
        let selected_generation = registry.register_generation(selected);
        let sibling_generation = registry.register_generation(sibling.clone());
        let equal_selected = UnscopedCancellationIdentity::new(Collision(1)).in_scope(scope);

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
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        assert_eq!(
            UnscopedCancellationIdentity::new(7_u64).in_scope(scopes.root()),
            UnscopedCancellationIdentity::new(7_u64).in_scope(scopes.root())
        );
    }

    #[test]
    fn diagnostics_redact_non_debug_values_without_changing_identity() {
        #[derive(Hash, PartialEq, Eq)]
        struct PrivateId(&'static str);

        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        let identity = UnscopedCancellationIdentity::new(PrivateId("private-account-4821"))
            .in_scope(scopes.root());
        let diagnostic = format!("{identity:?}");
        assert!(diagnostic.starts_with("CancellationIdentity"));
        assert!(!diagnostic.contains("private-account-4821"));
        assert_eq!(
            identity,
            UnscopedCancellationIdentity::new(PrivateId("private-account-4821"))
                .in_scope(scopes.root())
        );
        assert_ne!(
            identity,
            UnscopedCancellationIdentity::new(PrivateId("private-account-9302"))
                .in_scope(scopes.root())
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
            UnscopedCancellationIdentity::new(7_u8).in_scope(first),
            UnscopedCancellationIdentity::new(7_u8).in_scope(second)
        );
        assert_ne!(
            UnscopedCancellationIdentity::new(7_u8).in_scope(root),
            UnscopedCancellationIdentity::new(7_u8).in_scope(first)
        );
        assert_ne!(
            UnscopedCancellationIdentity::new(7_u8).in_scope(first),
            UnscopedCancellationIdentity::new(7_u8).in_scope(nested)
        );
    }

    #[cfg(feature = "test-util")]
    #[test]
    fn identity_reports_type_without_storing_metadata() {
        let (_lifecycle, scopes) = ScopeRegistry::for_test();
        assert_eq!(
            UnscopedCancellationIdentity::new(7_u64).in_scope(scopes.root()).type_name(),
            std::any::type_name::<u64>()
        );
    }
}
}
