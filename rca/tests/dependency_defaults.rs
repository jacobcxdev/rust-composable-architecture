//! Checks explicit default selection and recovery of rejected typed registrations.

use rca::{DependencyDefaultPolicy, DependencyDefaults, DependencyKey, DependencyValues};

/// Uses an optional value so an explicit `None` can mask a registered default.
struct Name;
impl DependencyKey for Name {
    type Value = Option<&'static str>;
}

#[test]
fn facade_preview_policy_selects_owned_fixture_and_none_masks_it() {
    let mut values = DependencyValues::for_policy(DependencyDefaultPolicy::Preview);
    values
        .register_defaults(
            DependencyDefaults::<Name>::new()
                .with_live(|| panic!("live must not run"))
                .with_test(|| Some("test"))
                .with_preview(|| Some("preview")),
        )
        .unwrap();
    assert_eq!(*values.get_or_insert::<Name>(), Some("preview"));
    let mut masked = values.overriding::<Name>(None);
    assert_eq!(*masked.get_or_insert::<Name>(), None);
    assert_eq!(*values.get_or_insert::<Name>(), Some("preview"));
}

#[test]
fn facade_duplicate_registration_recovers_original_typed_bundle() {
    let mut first = DependencyValues::new();
    first
        .register_defaults(DependencyDefaults::<Name>::new().with_live(|| Some("first")))
        .unwrap();
    let error: rca::DependencyRegistrationError<Name> = first
        .register_defaults(DependencyDefaults::<Name>::new().with_live(|| Some("second")))
        .unwrap_err();
    assert!(error.is_duplicate());
    let mut second = DependencyValues::new();
    second.register_defaults(error.into_defaults()).unwrap();
    assert_eq!(*first.get_or_insert::<Name>(), Some("first"));
    assert_eq!(*second.get_or_insert::<Name>(), Some("second"));
}
