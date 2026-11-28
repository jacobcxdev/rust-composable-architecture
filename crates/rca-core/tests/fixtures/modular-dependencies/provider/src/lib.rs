//! Supplies defaults without implementing foreign traits for foreign keys.

use modular_interface::{Client, ClientKey};
use rca_core::DependencyValues;
use rca_core::{DependencyDefaults, DependencyRegistrationError};

/// Registers explicit constructors in an environment owned by the caller.
///
/// # Errors
/// Recovers the submitted bundle if defaults were already registered or initialised.
pub fn register(
    values: &mut DependencyValues,
) -> Result<(), DependencyRegistrationError<ClientKey>> {
    values.register_defaults(
        DependencyDefaults::<ClientKey>::new()
            .with_live(|| Client::new(|| "production"))
            .with_test(|| Client::new(|| "fixture"))
            .with_preview(|| Client::new(|| "preview")),
    )
}

#[test]
fn independently_owned_provider_can_register_foreign_interface_defaults() {
    use rca_core::DependencyDefaultPolicy;
    for (policy, expected) in [
        (DependencyDefaultPolicy::Live, "production"),
        (DependencyDefaultPolicy::Test, "fixture"),
        (DependencyDefaultPolicy::Preview, "preview"),
    ] {
        let mut values = DependencyValues::for_policy(policy);
        register(&mut values).unwrap();
        assert_eq!(values.get_or_insert::<ClientKey>().call(), expected);
    }
}
