//! Checks that generated paths preserve enum shape, ownership and original storage.

use std::rc::Rc;

use rca_macros::{ActionPaths, StatePaths};

/// Exposes accidental cloning while retaining an independently checked payload value.
struct MoveOnly(Rc<()>, usize);

#[test]
fn aliased_optional_and_keyed_state_paths_borrow_and_mutate_original_storage() {
    type Maybe<T> = Option<T>;
    type Rows<T> = rca::KeyedState<u8, T>;

    #[derive(StatePaths)]
    struct AliasedState {
        #[rca(optional)]
        detail: Maybe<MoveOnly>,
        #[rca(keyed)]
        rows: Rows<MoveOnly>,
    }

    let mut state = AliasedState {
        detail: Some(MoveOnly(Rc::new(()), 1)),
        rows: [(7, MoveOnly(Rc::new(()), 3)), (9, MoveOnly(Rc::new(()), 5))]
            .into_iter()
            .collect(),
    };
    let detail_owner = Rc::downgrade(&state.detail.as_ref().unwrap().0);
    let row_owner = Rc::downgrade(&state.rows.get(&7).unwrap().0);
    let detail = AliasedStatePaths::detail();
    let rows = AliasedStatePaths::rows();
    assert!(std::ptr::eq(
        detail.get(&state).unwrap(),
        state.detail.as_ref().unwrap()
    ));
    assert!(std::ptr::eq(rows.get(&state), &raw const state.rows));
    assert!(std::ptr::eq(
        rows.get(&state).get(&7).unwrap(),
        state.rows.get(&7).unwrap()
    ));

    detail.get_mut(&mut state).unwrap().1 = 11;
    rows.get_mut(&mut state).get_mut(&7).unwrap().1 = 13;
    assert_eq!(state.detail.as_ref().unwrap().1, 11);
    assert_eq!(state.rows.get(&7).unwrap().1, 13);
    assert_eq!(state.rows.get(&9).unwrap().1, 5);
    assert!(rows.get(&state).ids().copied().eq([7, 9]));
    assert_eq!(Rc::strong_count(&state.rows.get(&7).unwrap().0), 1);

    drop(state.detail.take());
    assert!(detail.get(&state).is_none());
    assert!(detail.get_mut(&mut state).is_none());
    assert!(detail_owner.upgrade().is_none());
    assert_eq!(rows.get(&state).get(&7).unwrap().1, 13);
    drop(state);
    assert!(row_owner.upgrade().is_none());
}

#[test]
fn aliased_keyed_action_paths_round_trip_move_only_key_and_payload() {
    struct Key(Box<usize>);
    type RowAction = rca::KeyedAction<Key, MoveOnly>;

    #[derive(ActionPaths)]
    enum AliasedAction {
        #[rca(keyed)]
        Row(RowAction),
        Other,
    }

    let key = Key(Box::new(7));
    let key_address = std::ptr::from_ref(key.0.as_ref());
    let payload = MoveOnly(Rc::new(()), 17);
    let payload_owner = Rc::downgrade(&payload.0);
    let path = AliasedActionPaths::row();
    let action = path.embed(key, payload);
    let (key, payload) = path.extract(&action).unwrap();
    let AliasedAction::Row(original) = &action else {
        panic!("keyed path embedded the wrong variant");
    };
    assert!(std::ptr::eq(key, original.id()));
    assert!(std::ptr::eq(payload, original.action()));
    assert_eq!(std::ptr::from_ref(key.0.as_ref()), key_address);
    assert_eq!((*key.0, payload.1), (7, 17));
    assert_eq!(Rc::strong_count(&payload.0), 1);
    assert!(path.extract(&AliasedAction::Other).is_none());

    let AliasedAction::Row(original) = action else {
        panic!("keyed action changed variant during borrowing");
    };
    let (key, payload) = original.into_parts();
    let restored = path.embed(key, payload);
    let (key, payload) = path.extract(&restored).unwrap();
    assert_eq!(std::ptr::from_ref(key.0.as_ref()), key_address);
    assert_eq!((*key.0, payload.1), (7, 17));
    assert_eq!(Rc::strong_count(&payload.0), 1);
    drop(restored);
    assert!(payload_owner.upgrade().is_none());
}

/// Requires namespace formatting to propagate the first writer failure.
fn assert_debug_error(value: &impl std::fmt::Debug) {
    /// Counts writes while refusing every string.
    struct Reject(usize);
    impl std::fmt::Write for Reject {
        fn write_str(&mut self, _: &str) -> std::fmt::Result {
            self.0 += 1;
            Err(std::fmt::Error)
        }
    }
    let mut writer = Reject(0);
    assert!(std::fmt::write(&mut writer, format_args!("{value:?}")).is_err());
    assert_eq!(writer.0, 1);
}

/// Exercises explicit discriminants across tuple, named, single and unit actions.
#[derive(ActionPaths)]
#[repr(u8)]
enum DiscriminatedAction {
    Pair(u8, u16) = 1,
    Named { first: u8, second: u16 } = 2,
    Single(u8) = 3,
    Idle = 4,
}

/// Exercises mutable paths without changing explicit enum discriminants.
#[derive(StatePaths)]
#[repr(u8)]
enum DiscriminatedState {
    Pair(u8, u16) = 1,
    Named { first: u8, second: u16 } = 2,
    Single(u8) = 3,
    Idle = 4,
}

#[test]
fn valid_explicit_discriminants_preserve_payload_paths() {
    let pair = DiscriminatedActionPaths::pair().embed((1, 2));
    assert_eq!(
        DiscriminatedActionPaths::pair().extract(&pair),
        Some((&1, &2))
    );
    let named = DiscriminatedActionPaths::named().embed((3, 4));
    assert_eq!(
        DiscriminatedActionPaths::named().extract(&named),
        Some((&3, &4))
    );
    let single = DiscriminatedActionPaths::single().embed(5);
    assert_eq!(
        DiscriminatedActionPaths::single().extract(&single),
        Some(&5)
    );
    assert_eq!(
        DiscriminatedActionPaths::idle().extract(&DiscriminatedAction::Idle),
        Some(())
    );
    assert!(matches!(
        DiscriminatedActionPaths::idle().embed(()),
        DiscriminatedAction::Idle
    ));
    assert!(
        DiscriminatedActionPaths::pair()
            .extract(&DiscriminatedAction::Idle)
            .is_none()
    );
    assert!(
        DiscriminatedActionPaths::named()
            .extract(&DiscriminatedAction::Idle)
            .is_none()
    );
    assert!(
        DiscriminatedActionPaths::single()
            .extract(&DiscriminatedAction::Idle)
            .is_none()
    );
    assert!(DiscriminatedActionPaths::idle().extract(&single).is_none());
}

#[test]
fn state_discriminants_preserve_payload_paths_and_mutation() {
    let mut pair = DiscriminatedStatePaths::pair().embed((6, 7));
    let (first, second) = DiscriminatedStatePaths::pair().get_mut(&mut pair).unwrap();
    *first = 8;
    *second = 9;
    assert_eq!(DiscriminatedStatePaths::pair().get(&pair), Some((&8, &9)));
    let mut named = DiscriminatedStatePaths::named().embed((10, 11));
    assert_eq!(
        DiscriminatedStatePaths::named().get(&named),
        Some((&10, &11))
    );
    let (first, second) = DiscriminatedStatePaths::named()
        .get_mut(&mut named)
        .unwrap();
    *first = 13;
    *second = 14;
    assert_eq!(
        DiscriminatedStatePaths::named().get(&named),
        Some((&13, &14))
    );
    let mut single = DiscriminatedState::Single(12);
    assert_eq!(DiscriminatedStatePaths::single().get(&single), Some(&12));
    *DiscriminatedStatePaths::single()
        .get_mut(&mut single)
        .unwrap() = 15;
    assert_eq!(DiscriminatedStatePaths::single().get(&single), Some(&15));
    assert_eq!(
        DiscriminatedStatePaths::idle().get(&DiscriminatedState::Idle),
        Some(())
    );
    let mut idle = DiscriminatedStatePaths::idle().embed(());
    assert!(matches!(idle, DiscriminatedState::Idle));
    assert_eq!(DiscriminatedStatePaths::idle().get_mut(&mut idle), Some(()));
    assert!(DiscriminatedStatePaths::pair().get(&idle).is_none());
    assert!(DiscriminatedStatePaths::pair().get_mut(&mut idle).is_none());
    assert!(DiscriminatedStatePaths::named().get(&idle).is_none());
    assert!(
        DiscriminatedStatePaths::named()
            .get_mut(&mut idle)
            .is_none()
    );
    assert!(DiscriminatedStatePaths::single().get(&idle).is_none());
    assert!(
        DiscriminatedStatePaths::single()
            .get_mut(&mut idle)
            .is_none()
    );
    assert!(DiscriminatedStatePaths::idle().get(&single).is_none());
    assert!(
        DiscriminatedStatePaths::idle()
            .get_mut(&mut single)
            .is_none()
    );
    assert_eq!(
        format!("{DiscriminatedActionPaths:?}"),
        "DiscriminatedActionPaths"
    );
    assert_eq!(
        format!("{DiscriminatedStatePaths:?}"),
        "DiscriminatedStatePaths"
    );
    assert_debug_error(&DiscriminatedActionPaths);
    assert_debug_error(&DiscriminatedStatePaths);
}

/// Provides two payload-free cases whose paths must remain distinct.
#[derive(ActionPaths)]
enum UnitAction {
    Idle,
    Other,
}

/// Provides two payload-free cases for shared and mutable state projection.
#[derive(StatePaths)]
enum UnitState {
    Idle,
    Other,
}

/// Combines borrowed, move-only, const-generic and recursive action payloads.
#[derive(ActionPaths)]
enum AggregateAction<'a, T, const N: usize> {
    Tuple(T, &'a str, [u8; N]),
    Named {
        value: T,
        label: &'a str,
    },
    Recursive {
        child: Box<Self>,
        tag: usize,
    },
    #[cfg(any())]
    Disabled(MissingType, MissingType),
    Other,
}

/// Combines heterogeneous state fields that must remain separately mutable.
#[derive(StatePaths)]
enum AggregateState<'a, T, const N: usize> {
    Tuple(T, &'a str, [u8; N]),
    Named {
        value: T,
        label: &'a str,
    },
    Recursive {
        child: Box<Self>,
        tag: usize,
    },
    #[cfg(any())]
    Disabled(MissingType, MissingType),
    Other,
}

#[test]
fn unit_cases_match_and_embed_only_the_selected_variant() {
    let action = UnitActionPaths::idle();
    assert_eq!(action.extract(&UnitAction::Idle), Some(()));
    assert_eq!(action.extract(&UnitAction::Other), None);
    assert!(matches!(action.embed(()), UnitAction::Idle));
    let state = UnitStatePaths::idle();
    assert_eq!(state.get(&UnitState::Idle), Some(()));
    assert_eq!(state.get(&UnitState::Other), None);
    assert_eq!(state.get_mut(&mut UnitState::Idle), Some(()));
    assert_eq!(state.get_mut(&mut UnitState::Other), None);
    assert!(matches!(state.embed(()), UnitState::Idle));
    let action = UnitActionPaths::other();
    assert!(matches!(action.embed(()), UnitAction::Other));
    assert_eq!(action.extract(&UnitAction::Other), Some(()));
    assert_eq!(action.extract(&UnitAction::Idle), None);
    let state = UnitStatePaths::other();
    assert!(matches!(state.embed(()), UnitState::Other));
    assert_eq!(state.get(&UnitState::Other), Some(()));
    assert_eq!(state.get(&UnitState::Idle), None);
    assert_eq!(state.get_mut(&mut UnitState::Other), Some(()));
    assert_eq!(state.get_mut(&mut UnitState::Idle), None);
    assert_eq!(format!("{UnitActionPaths:?}"), "UnitActionPaths");
    assert_eq!(format!("{UnitStatePaths:?}"), "UnitStatePaths");
    assert_debug_error(&UnitActionPaths);
    assert_debug_error(&UnitStatePaths);
}

// Empty tuple/named cases have no fields but retain distinct enum syntax and identity.
#[test]
#[expect(
    clippy::empty_enum_variants_with_brackets,
    reason = "Empty tuple and named forms are distinct macro input shapes."
)]
fn empty_tuple_and_named_cases_preserve_embedding_and_projection_shapes() {
    #[derive(ActionPaths)]
    enum EmptyAction {
        Tuple(),
        Named {},
    }
    #[derive(StatePaths)]
    enum EmptyState {
        Tuple(),
        Named {},
    }

    let tuple = EmptyActionPaths::tuple();
    let named = EmptyActionPaths::named();
    assert!(matches!(tuple.embed(()), EmptyAction::Tuple()));
    assert!(matches!(named.embed(()), EmptyAction::Named {}));
    assert_eq!(tuple.extract(&EmptyAction::Tuple()), Some(()));
    assert_eq!(tuple.extract(&EmptyAction::Named {}), None);
    assert_eq!(named.extract(&EmptyAction::Named {}), Some(()));
    assert_eq!(named.extract(&EmptyAction::Tuple()), None);

    let tuple = EmptyStatePaths::tuple();
    let named = EmptyStatePaths::named();
    assert!(matches!(tuple.embed(()), EmptyState::Tuple()));
    assert!(matches!(named.embed(()), EmptyState::Named {}));
    assert_eq!(tuple.get(&EmptyState::Tuple()), Some(()));
    assert_eq!(tuple.get(&EmptyState::Named {}), None);
    assert_eq!(named.get(&EmptyState::Named {}), Some(()));
    assert_eq!(named.get(&EmptyState::Tuple()), None);
    assert_eq!(tuple.get_mut(&mut EmptyState::Tuple()), Some(()));
    assert_eq!(tuple.get_mut(&mut EmptyState::Named {}), None);
    assert_eq!(named.get_mut(&mut EmptyState::Named {}), Some(()));
    assert_eq!(named.get_mut(&mut EmptyState::Tuple()), None);
    assert_eq!(format!("{EmptyActionPaths:?}"), "EmptyActionPaths");
    assert_eq!(format!("{EmptyStatePaths:?}"), "EmptyStatePaths");
    assert_debug_error(&EmptyActionPaths);
    assert_debug_error(&EmptyStatePaths);
}

#[test]
fn action_tuple_projection_borrows_original_heterogeneous_fields() {
    let label = String::from("local");
    let path = AggregateActionPaths::<MoveOnly, 2>::tuple();
    let root = path.embed((MoveOnly(Rc::new(()), 7), label.as_str(), [3, 4]));
    let (value, borrowed_label, bytes) = path.extract(&root).unwrap();
    let AggregateAction::Tuple(direct_value, direct_label, direct_bytes) = &root else {
        panic!("wrong embedded variant");
    };
    assert!(std::ptr::eq(value, direct_value));
    assert!(std::ptr::eq(borrowed_label, direct_label));
    assert!(std::ptr::eq(bytes, direct_bytes));
    assert_eq!((value.1, *borrowed_label, bytes), (7, "local", &[3, 4]));
    assert_eq!(Rc::strong_count(&value.0), 1);
    assert!(path.extract(&AggregateAction::Other).is_none());
}

#[test]
fn action_named_projection_uses_declaration_order_and_exact_field_names() {
    let label = String::from("named");
    let path = AggregateActionPaths::<MoveOnly, 0>::named();
    let root = path.embed((MoveOnly(Rc::new(()), 8), label.as_str()));
    let (value, borrowed_label) = path.extract(&root).unwrap();
    let AggregateAction::Named {
        value: direct,
        label: direct_label,
    } = &root
    else {
        panic!("wrong embedded variant");
    };
    assert!(std::ptr::eq(value, direct));
    assert!(std::ptr::eq(borrowed_label, direct_label));
    assert_eq!((value.1, *borrowed_label), (8, "named"));
    assert!(path.extract(&AggregateAction::Other).is_none());
}

#[test]
fn state_tuple_projection_returns_simultaneous_disjoint_mutable_fields() {
    let label = String::from("before");
    let path = AggregateStatePaths::<MoveOnly, 2>::tuple();
    let mut root = path.embed((MoveOnly(Rc::new(()), 1), label.as_str(), [2, 3]));
    let (value, borrowed_label, bytes) = path.get(&root).unwrap();
    let AggregateState::Tuple(direct, direct_label, direct_bytes) = &root else {
        panic!("wrong embedded variant");
    };
    assert!(std::ptr::eq(value, direct));
    assert!(std::ptr::eq(borrowed_label, direct_label));
    assert!(std::ptr::eq(bytes, direct_bytes));
    let (value, borrowed_label, bytes) = path.get_mut(&mut root).unwrap();
    value.1 = 9;
    *borrowed_label = "after";
    bytes[0] = 6;
    let mut other = AggregateState::Other;
    assert!(path.get(&other).is_none());
    assert!(path.get_mut(&mut other).is_none());
    let AggregateState::Tuple(value, actual_label, bytes) = root else {
        panic!("wrong stored variant");
    };
    assert_eq!((value.1, actual_label, bytes), (9, "after", [6, 3]));
    assert_eq!(Rc::strong_count(&value.0), 1);
}

#[test]
fn state_named_projection_mutates_actual_storage_in_declaration_order() {
    let label = String::from("named");
    let path = AggregateStatePaths::<MoveOnly, 0>::named();
    let mut root = path.embed((MoveOnly(Rc::new(()), 2), label.as_str()));
    let (value, borrowed_label) = path.get_mut(&mut root).unwrap();
    value.1 = 4;
    *borrowed_label = "changed";
    let (value, label) = path.get(&root).unwrap();
    assert_eq!((value.1, *label), (4, "changed"));
    let mut other = AggregateState::Other;
    assert!(path.get(&other).is_none());
    assert!(path.get_mut(&mut other).is_none());
    let AggregateState::Named { value, label } = root else {
        panic!("wrong stored variant");
    };
    assert_eq!((value.1, label), (4, "changed"));
}

#[test]
fn recursive_self_fields_keep_the_original_root_type() {
    let action_path = AggregateActionPaths::<MoveOnly, 0>::recursive();
    let action = action_path.embed((Box::new(AggregateAction::Other), 5));
    let (child, tag) = action_path.extract(&action).unwrap();
    assert!(matches!(child.as_ref(), AggregateAction::Other));
    assert_eq!(*tag, 5);
    assert!(action_path.extract(&AggregateAction::Other).is_none());
    let state_path = AggregateStatePaths::<MoveOnly, 0>::recursive();
    let mut state = state_path.embed((Box::new(AggregateState::Other), 6));
    let (child, tag) = state_path.get_mut(&mut state).unwrap();
    assert!(matches!(child.as_ref(), AggregateState::Other));
    *tag = 7;
    assert_eq!(*state_path.get(&state).unwrap().1, 7);
    let mut other = AggregateState::Other;
    assert!(state_path.get(&other).is_none());
    assert!(state_path.get_mut(&mut other).is_none());
    let action_other = AggregateActionPaths::<MoveOnly, 0>::other();
    assert!(matches!(action_other.embed(()), AggregateAction::Other));
    assert_eq!(action_other.extract(&AggregateAction::Other), Some(()));
    assert!(action_other.extract(&action).is_none());
    let state_other = AggregateStatePaths::<MoveOnly, 0>::other();
    assert!(matches!(state_other.embed(()), AggregateState::Other));
    assert_eq!(state_other.get(&other), Some(()));
    assert_eq!(state_other.get_mut(&mut other), Some(()));
    assert!(state_other.get(&state).is_none());
    assert!(state_other.get_mut(&mut state).is_none());
}

#[test]
fn zero_sized_path_metadata_does_not_require_payload_traits() {
    fn clone_and_default<P: Clone + Default>(path: &P) -> (P, P) {
        (Clone::clone(path), P::default())
    }
    fn assert_copy_and_debug<P: Copy + std::fmt::Debug>(path: P) -> P {
        path
    }
    let action = assert_copy_and_debug(AggregateActionPaths::<MoveOnly, 1>::tuple());
    let state = assert_copy_and_debug(AggregateStatePaths::<MoveOnly, 1>::named());
    assert_eq!(format!("{action:?}"), "AggregateActionPath");
    assert_eq!(format!("{state:?}"), "AggregateStatePath");
    assert_eq!(std::mem::size_of_val(&action), 0);
    assert_eq!(std::mem::size_of_val(&state), 0);
    let (action_clone, action_default) = clone_and_default(&action);
    let root = action.embed((MoveOnly(Rc::new(()), 7), "local", [3]));
    let AggregateAction::Tuple(value, label, bytes) = &root else {
        panic!("tuple route must retain its case");
    };
    for path in [action_clone, action_default] {
        let projected = path.extract(&root).unwrap();
        assert!(std::ptr::eq(projected.0, value));
        assert!(std::ptr::eq(projected.1, label));
        assert!(std::ptr::eq(projected.2, bytes));
        assert_eq!(
            (projected.0.1, *projected.1, *projected.2),
            (7, "local", [3])
        );
        assert!(path.extract(&AggregateAction::Other).is_none());
    }
    assert_eq!(Rc::strong_count(&value.0), 1);

    let (state_clone, state_default) = clone_and_default(&state);
    let mut root = state.embed((MoveOnly(Rc::new(()), 2), "before"));
    let (value, label) = state_clone.get_mut(&mut root).unwrap();
    value.1 = 9;
    *label = "after";
    let (value, label) = state_default.get(&root).unwrap();
    assert_eq!((value.1, *label), (9, "after"));
    assert_eq!(Rc::strong_count(&value.0), 1);
    let mut other = AggregateState::Other;
    for path in [state_clone, state_default] {
        assert!(path.get(&other).is_none());
        assert!(path.get_mut(&mut other).is_none());
    }
}
