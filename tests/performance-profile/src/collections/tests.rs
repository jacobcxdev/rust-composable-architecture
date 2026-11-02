//! Mutation controls for work completion, collection order and payload release.

use super::*;

/// Selects one fixture so mutations have a single, independently checked owner set.
fn config(name: &str, size: &str) -> Config {
    Config::parse([name, size, "1"].map(str::to_owned).into_iter()).unwrap()
}

#[test]
fn skipped_empty_or_singleton_sort_is_not_completion() {
    for size in ["0", "1"] {
        let config = config("collection-sort-sorted", size);
        let probe = Rc::new(Probe::default());
        let mut fixtures = fixtures(config, &probe).unwrap();
        assert!(validate(config, &fixtures, &probe, 1).is_err());
        let visits = profile_collection_sort_work(&mut fixtures, config);
        assert_eq!(validate(config, &fixtures, &probe, visits), Ok(config.size));
        drop(fixtures);
        assert_eq!(released(&probe), Ok(()));
    }
}

#[test]
fn wrong_insertion_order_and_missing_input_are_rejected() {
    let config = config("collection-insert-head", "16");
    let probe = Rc::new(Probe::default());
    let mut fixtures = fixtures(config, &probe).unwrap();
    let incoming = fixtures[0].input.take();
    let visits = profile_collection_insert_work(&mut fixtures, config);
    assert!(validate(config, &fixtures, &probe, visits).is_err());
    fixtures[0].input = incoming;
    let visits = profile_collection_insert_work(&mut fixtures, config);
    assert_eq!(validate(config, &fixtures, &probe, visits), Ok(17));
    fixtures[0].rows.move_index(0, 16);
    assert!(validate(config, &fixtures, &probe, visits).is_err());
    drop(fixtures);
    assert_eq!(released(&probe), Ok(()));
}

#[test]
fn wrong_comparison_and_driver_counts_are_rejected() {
    let config = config("collection-sort-ties", "16");
    let probe = Rc::new(Probe::default());
    let mut fixtures = fixtures(config, &probe).unwrap();
    let visits = profile_collection_sort_work(&mut fixtures, config);
    assert_eq!(validate(config, &fixtures, &probe, visits), Ok(16));
    assert!(validate(config, &fixtures, &probe, visits + 1).is_err());
    fixtures[0].comparisons += 1;
    assert!(validate(config, &fixtures, &probe, visits).is_err());
    drop(fixtures);
    assert_eq!(released(&probe), Ok(()));
}

#[test]
fn retained_value_is_rejected_then_released_without_leaking() {
    let config = config("collection-insert-tail", "0");
    let probe = Rc::new(Probe::default());
    let mut fixtures = fixtures(config, &probe).unwrap();
    let visits = profile_collection_insert_work(&mut fixtures, config);
    assert_eq!(validate(config, &fixtures, &probe, visits), Ok(1));
    let retained = fixtures[0].rows.remove(&0).unwrap();
    drop(fixtures);
    assert_eq!(probe.dropped.get(), 0);
    assert_eq!(released(&probe), Err("collection owners retained"));
    drop(retained);
    assert_eq!(probe.dropped.get(), 1);
    assert_eq!(released(&probe), Ok(()));
}
