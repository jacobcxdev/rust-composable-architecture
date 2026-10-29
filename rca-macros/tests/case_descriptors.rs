//! Checks case identity, borrowed views and diagnostics without payload trait bounds.

use std::rc::Rc;

use rca_macros::{ActionPaths, CaseDescriptors};

/// Makes extra payload bounds or retained clones visible to the test.
struct MoveOnly(Rc<()>, usize);

#[test]
#[expect(
    non_camel_case_types,
    reason = "raw keyword roots retain their spelling in generated type names"
)]
fn raw_and_unicode_roots_preserve_generated_names_and_borrowed_payloads() {
    #[derive(CaseDescriptors)]
    enum r#type {
        Value(MoveOnly),
    }

    #[derive(CaseDescriptors)]
    enum État {
        Value(MoveOnly),
    }

    let raw = r#type::Value(MoveOnly(Rc::new(()), 7));
    assert_eq!(typeCases::ALL, &[typeCase::Value]);
    assert_eq!(typeCases::case(&raw), typeCase::Value);
    let r#type::Value(original) = &raw;
    let typeCaseRef::Value(projected) = typeCases::project(&raw);
    assert!(std::ptr::eq(projected, original));
    assert_eq!(projected.1, 7);

    let unicode = État::Value(MoveOnly(Rc::new(()), 23));
    assert_eq!(ÉtatCases::ALL, &[ÉtatCase::Value]);
    assert_eq!(ÉtatCases::case(&unicode), ÉtatCase::Value);
    let État::Value(original) = &unicode;
    let ÉtatCaseRef::Value(projected) = ÉtatCases::project(&unicode);
    assert!(std::ptr::eq(projected, original));
    assert_eq!(projected.1, 23);
}

/// Separates path selection from the complete catalogue of enabled cases.
#[derive(ActionPaths, CaseDescriptors)]
enum Event<'a, T, const N: usize> {
    Idle,
    Single(T),
    Pair(T, &'a str, [u8; N]),
    Named {
        value: T,
        text: &'a str,
    },
    Recursive(Box<Self>),
    #[rca(skip)]
    Skipped(T),
    #[cfg(any())]
    Disabled(MissingType),
}

#[test]
fn catalogue_keeps_declaration_order_including_path_skipped_cases() {
    assert_eq!(
        EventCases::<MoveOnly, 0>::ALL,
        &[
            EventCase::Idle,
            EventCase::Single,
            EventCase::Pair,
            EventCase::Named,
            EventCase::Recursive,
            EventCase::Skipped
        ]
    );
    let root = Event::<MoveOnly, 0>::Skipped(MoveOnly(Rc::new(()), 5));
    assert_eq!(EventCases::case(&root), EventCase::Skipped);
    let EventCaseRef::Skipped(value) = EventCases::project(&root) else {
        panic!("wrong case")
    };
    assert_eq!(value.1, 5);
}

/// Checks every case at each tested array size with literal expected output.
fn check_enabled_case_views<const N: usize>() {
    /// Counts writes while rejecting the first attempted output.
    struct RejectingWriter {
        /// Number of attempted writes, even when formatting fails.
        writes: usize,
    }

    impl std::fmt::Write for RejectingWriter {
        fn write_str(&mut self, _value: &str) -> std::fmt::Result {
            self.writes += 1;
            Err(std::fmt::Error)
        }
    }

    let cases = [
        (
            Event::<MoveOnly, N>::Idle,
            EventCase::Idle,
            "Idle",
            "EventCaseRef::Idle",
        ),
        (
            Event::Single(MoveOnly(Rc::new(()), 1)),
            EventCase::Single,
            "Single",
            "EventCaseRef::Single",
        ),
        (
            Event::Pair(MoveOnly(Rc::new(()), 2), "pair", [7; N]),
            EventCase::Pair,
            "Pair",
            "EventCaseRef::Pair",
        ),
        (
            Event::Named {
                value: MoveOnly(Rc::new(()), 3),
                text: "named",
            },
            EventCase::Named,
            "Named",
            "EventCaseRef::Named",
        ),
        (
            Event::Recursive(Box::new(Event::Idle)),
            EventCase::Recursive,
            "Recursive",
            "EventCaseRef::Recursive",
        ),
        (
            Event::Skipped(MoveOnly(Rc::new(()), 4)),
            EventCase::Skipped,
            "Skipped",
            "EventCaseRef::Skipped",
        ),
    ];

    for (root, expected_case, expected_label, expected_debug) in cases {
        let case = EventCases::case(&root);
        assert_eq!(case, expected_case);
        assert_eq!(case.label(), Some(expected_label));
        let view = EventCases::project(&root);
        assert_eq!(format!("{view:?}"), expected_debug);

        let mut writer = RejectingWriter { writes: 0 };
        assert_eq!(
            std::fmt::write(&mut writer, format_args!("{view:?}")),
            Err(std::fmt::Error)
        );
        assert_eq!(writer.writes, 1, "case {expected_label}");
    }
}

#[test]
fn every_enabled_case_keeps_its_label_and_propagates_view_formatting_errors() {
    check_enabled_case_views::<0>();
    check_enabled_case_views::<2>();
}

#[test]
fn typed_view_borrows_original_heterogeneous_fields_without_payload_bounds() {
    let text = String::from("local");
    let root = Event::Pair(MoveOnly(Rc::new(()), 7), text.as_str(), [1, 2]);
    let EventCaseRef::Pair(value, label, bytes) = EventCases::project(&root) else {
        panic!("wrong case")
    };
    let Event::Pair(original, original_label, original_bytes) = &root else {
        panic!("wrong root")
    };
    assert!(std::ptr::eq(value, original));
    assert!(std::ptr::eq(label, original_label));
    assert!(std::ptr::eq(bytes, original_bytes));
    assert_eq!((value.1, *label, bytes), (7, "local", &[1, 2]));
    assert_eq!(Rc::strong_count(&value.0), 1);
    assert_eq!(
        format!("{:?}", EventCases::project(&root)),
        "EventCaseRef::Pair"
    );
}

#[test]
fn named_recursive_single_and_unit_views_preserve_actual_cases() {
    let named = Event::<MoveOnly, 0>::Named {
        value: MoveOnly(Rc::new(()), 8),
        text: "named",
    };
    let EventCaseRef::Named { value, text } = EventCases::project(&named) else {
        panic!("wrong case")
    };
    assert_eq!((value.1, *text), (8, "named"));
    let recursive = Event::<MoveOnly, 0>::Recursive(Box::new(Event::Idle));
    let EventCaseRef::Recursive(child) = EventCases::project(&recursive) else {
        panic!("wrong case")
    };
    assert!(matches!(child.as_ref(), Event::Idle));
    let single = Event::<MoveOnly, 0>::Single(MoveOnly(Rc::new(()), 9));
    assert!(matches!(EventCases::project(&single), EventCaseRef::Single(value) if value.1 == 9));
    assert!(matches!(
        EventCases::project(&Event::<MoveOnly, 0>::Idle),
        EventCaseRef::Idle
    ));
}

/// Gives distinct cases equal payload types and values.
#[derive(CaseDescriptors)]
enum SamePayload {
    First(u8),
    Second(u8),
}

#[test]
fn labels_are_optional_diagnostics_not_case_identity() {
    let first = SamePayloadCases::case(&SamePayload::First(1));
    let second = SamePayloadCases::case(&SamePayload::Second(1));
    assert_ne!(first, second);
    assert_eq!(first.label(), Some("First"));
    assert_eq!(second.label(), Some("Second"));
    let custom = String::from("duplicate");
    let mut calls = 0;
    let first_label = first.label_with(|tag| {
        calls += 1;
        assert_eq!(tag, SamePayloadCase::First);
        Some(custom.as_str())
    });
    let second_label = second.label_with(|tag| {
        calls += 1;
        assert_eq!(tag, SamePayloadCase::Second);
        Some(custom.as_str())
    });
    assert_eq!(first_label, Some("duplicate"));
    assert_eq!(first_label, second_label);
    assert!(std::ptr::eq(first_label.unwrap(), custom.as_str()));
    assert!(std::ptr::eq(second_label.unwrap(), custom.as_str()));
    assert_eq!(calls, 2);

    assert_eq!(
        first.label_with(|tag| {
            calls += 1;
            assert_eq!(tag, SamePayloadCase::First);
            None
        }),
        None
    );
    assert_eq!(calls, 3);
    assert_ne!(first, second);
}

/// Checks that payload-free views need no synthetic borrow lifetime.
#[derive(CaseDescriptors)]
enum UnitOnly {
    Idle,
    Done,
}

/// Keeps valid numeric discriminants independent of case descriptors.
#[derive(CaseDescriptors)]
#[repr(u8)]
enum Explicit {
    Pair(u8, u16) = 1,
    Named { first: u8, second: u16 } = 2,
    Idle = 3,
}

#[test]
fn unit_only_and_valid_explicit_discriminants_need_no_extra_bounds() {
    assert_eq!(
        UnitOnlyCases::ALL,
        &[UnitOnlyCase::Idle, UnitOnlyCase::Done]
    );
    assert!(matches!(
        UnitOnlyCases::project(&UnitOnly::Idle),
        UnitOnlyCaseRef::Idle
    ));
    assert_eq!(UnitOnlyCases::case(&UnitOnly::Done), UnitOnlyCase::Done);
    assert_eq!(UnitOnlyCases::case(&UnitOnly::Idle), UnitOnlyCase::Idle);
    assert!(matches!(
        UnitOnlyCases::project(&UnitOnly::Done),
        UnitOnlyCaseRef::Done
    ));
    assert!(matches!(
        ExplicitCases::project(&Explicit::Pair(1, 2)),
        ExplicitCaseRef::Pair(&1, &2)
    ));
    assert!(matches!(
        ExplicitCases::project(&Explicit::Named {
            first: 3,
            second: 4
        }),
        ExplicitCaseRef::Named {
            first: &3,
            second: &4
        }
    ));
    assert_eq!(ExplicitCases::case(&Explicit::Idle), ExplicitCase::Idle);
    assert_eq!(
        ExplicitCases::case(&Explicit::Pair(1, 2)),
        ExplicitCase::Pair
    );
    assert_eq!(
        ExplicitCases::case(&Explicit::Named {
            first: 3,
            second: 4
        }),
        ExplicitCase::Named
    );
    assert!(matches!(
        ExplicitCases::project(&Explicit::Idle),
        ExplicitCaseRef::Idle
    ));
}

/// Ensures a disabled variant is absent from the generated catalogue.
#[derive(CaseDescriptors)]
enum DisabledPayload {
    Idle,
    #[r#cfg(any())]
    Disabled(u8),
}

/// Leaves unresolved payload types behind disabled field attributes.
#[derive(CaseDescriptors)]
enum ConditionalFields {
    Tuple(#[r#cfg_attr(all(), r#cfg(any()))] MissingType, u8),
    Named {
        #[r#cfg(any())]
        disabled: MissingType,
        enabled: u16,
    },
}

#[test]
fn cfg_disabled_payloads_and_fields_do_not_appear_in_views() {
    assert_eq!(DisabledPayloadCases::ALL, &[DisabledPayloadCase::Idle]);
    assert!(matches!(
        DisabledPayloadCases::project(&DisabledPayload::Idle),
        DisabledPayloadCaseRef::Idle
    ));
    assert!(matches!(
        ConditionalFieldsCases::project(&ConditionalFields::Tuple(3)),
        ConditionalFieldsCaseRef::Tuple(&3)
    ));
    assert!(matches!(
        ConditionalFieldsCases::project(&ConditionalFields::Named { enabled: 4 }),
        ConditionalFieldsCaseRef::Named { enabled: &4 }
    ));
}

/// Supplies an uninhabited domain without constructing an impossible value.
#[derive(CaseDescriptors)]
enum Empty {}

/// Distinguishes empty tuple and named variants from unit syntax.
#[derive(CaseDescriptors)]
#[expect(
    clippy::empty_enum_variants_with_brackets,
    reason = "valid empty payload syntax must survive the derive"
)]
enum EmptyShapes {
    Tuple(),
    Named {},
}

#[test]
fn empty_enum_catalogue_and_empty_payload_shapes_preserve_rust_domains() {
    let _: Option<Empty> = None;
    assert!(EmptyCases::ALL.is_empty());
    assert_eq!(
        EmptyShapesCases::ALL,
        &[EmptyShapesCase::Tuple, EmptyShapesCase::Named]
    );
    assert!(matches!(
        EmptyShapesCases::project(&EmptyShapes::Tuple()),
        EmptyShapesCaseRef::Tuple()
    ));
    assert!(matches!(
        EmptyShapesCases::project(&EmptyShapes::Named {}),
        EmptyShapesCaseRef::Named {}
    ));
}

#[test]
fn case_descriptors_compile_boundaries() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/case_descriptors/pass_*.rs");
    tests.compile_fail("tests/ui/case_descriptors/fail_*.rs");
}

#[test]
fn inhabited_tags_preserve_identity_hashing_and_debug_labels() {
    use std::hash::{DefaultHasher, Hash, Hasher};

    for (tag, expected_debug) in [
        (EventCase::Idle, "Idle"),
        (EventCase::Single, "Single"),
        (EventCase::Pair, "Pair"),
        (EventCase::Named, "Named"),
        (EventCase::Recursive, "Recursive"),
        (EventCase::Skipped, "Skipped"),
    ] {
        let cloned = Clone::clone(&tag);
        assert_eq!(cloned, tag);
        assert_eq!(format!("{cloned:?}"), expected_debug);

        let mut original_hash = DefaultHasher::new();
        let mut cloned_hash = DefaultHasher::new();
        tag.hash(&mut original_hash);
        cloned.hash(&mut cloned_hash);
        assert_eq!(original_hash.finish(), cloned_hash.finish());
    }
}
