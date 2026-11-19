//! Exercises injected application clients through the default façade exports.

use std::time::{Duration, SystemTime};

use rca::{ApplicationIds, CivilData, DependencyKey, DependencyValues, RandomBytes, WallTime};

/// Shares one application-owned sequence through a typed dependency key.
struct Ids;

impl DependencyKey for Ids {
    type Value = ApplicationIds<u32>;
}

#[test]
fn injected_application_values_keep_explicit_ownership_and_civil_data() {
    let mut environment = DependencyValues::new();
    environment.set::<Ids>(ApplicationIds::sequence([7, 7, 9]));
    let original = environment.get_or_insert::<Ids>().clone();
    let shared = original.clone();
    drop(environment);
    assert_eq!(original.next(), Some(7));
    assert_eq!(shared.next(), Some(7));
    assert_eq!(original.next(), Some(9));
    assert_eq!(shared.next(), None);

    let constant = ApplicationIds::constant(7);
    assert_eq!(constant.next(), Some(7));
    assert_eq!(constant.next(), Some(7));

    let random = RandomBytes::new(|output: &mut [u8]| -> Result<(), u8> {
        output.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        Ok(())
    });
    assert_eq!(random.next_u64().unwrap(), 0x0807_0605_0403_0201);

    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(17);
    assert_eq!(WallTime::constant(time).now(), time);
    let civil = CivilData::new("caller-calendar", "en-GB");
    assert_eq!(civil.calendar(), &"caller-calendar");
    assert_eq!(civil.locale(), &"en-GB");
}
