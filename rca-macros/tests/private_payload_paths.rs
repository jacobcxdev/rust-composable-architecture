//! Checks public paths whose valid enum inputs retain private, move-only payloads.

#![expect(
    private_interfaces,
    missing_debug_implementations,
    reason = "these valid public enums intentionally retain private move-only payloads"
)]

use rca_macros::{ActionPaths, StatePaths};

/// Keeps payload ownership private while the enclosing enum remains public.
struct Internal(String);

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

/// Exposes a single private action payload without adding Clone or Debug.
#[derive(ActionPaths)]
pub enum SingleAction {
    /// Owns the private payload.
    Value(Internal),
    /// Supplies a wrong-case control for the value path.
    Idle,
}

/// Exposes a single private state payload for in-place mutation.
#[derive(StatePaths)]
pub enum SingleState {
    /// Owns the private payload.
    Value(Internal),
    /// Supplies a wrong-case control for the value path.
    Idle,
}

/// Combines public action cases with private tuple and named payload fields.
#[derive(ActionPaths)]
pub enum Action {
    /// Retains heterogeneous tuple field ownership and order.
    Pair(Internal, usize),
    /// Retains the original named fields.
    Named { internal: Internal, count: usize },
    /// Supplies the wrong-case control.
    Idle,
}

/// Combines public state cases with separately mutable private payload fields.
#[derive(StatePaths)]
pub enum State {
    /// Retains heterogeneous tuple fields for simultaneous mutable borrowing.
    Pair(Internal, usize),
    /// Retains the original named fields.
    Named { internal: Internal, count: usize },
    /// Supplies the wrong-case control.
    Idle,
}

#[test]
fn public_one_field_paths_keep_private_payload_warning_only() {
    let action = SingleActionPaths::value().embed(Internal(String::from("single")));
    assert_eq!(
        SingleActionPaths::value().extract(&action).unwrap().0,
        "single"
    );
    let mut state = SingleState::Value(Internal(String::from("single")));
    SingleStatePaths::value()
        .get_mut(&mut state)
        .unwrap()
        .0
        .push_str(" changed");
    assert_eq!(
        SingleStatePaths::value().get(&state).unwrap().0,
        "single changed"
    );
    assert!(
        SingleActionPaths::value()
            .extract(&SingleAction::Idle)
            .is_none()
    );
    let idle = SingleActionPaths::idle();
    assert!(matches!(idle.embed(()), SingleAction::Idle));
    assert_eq!(idle.extract(&SingleAction::Idle), Some(()));
    assert!(idle.extract(&action).is_none());

    let mut idle = SingleState::Idle;
    assert!(SingleStatePaths::value().get(&idle).is_none());
    assert!(SingleStatePaths::value().get_mut(&mut idle).is_none());
    let path = SingleStatePaths::idle();
    assert!(matches!(path.embed(()), SingleState::Idle));
    assert_eq!(path.get(&idle), Some(()));
    assert_eq!(path.get_mut(&mut idle), Some(()));
    assert!(path.get(&state).is_none());
    assert!(path.get_mut(&mut state).is_none());
    assert_eq!(format!("{SingleActionPaths:?}"), "SingleActionPaths");
    assert_eq!(format!("{SingleStatePaths:?}"), "SingleStatePaths");
    assert_debug_error(&SingleActionPaths);
    assert_debug_error(&SingleStatePaths);
}

#[test]
fn public_aggregate_paths_keep_private_fields_borrowed_and_owned() {
    let path = ActionPaths::pair();
    let action = path.embed((Internal(String::from("tuple")), 1));
    let (internal, count) = path.extract(&action).unwrap();
    let Action::Pair(direct, direct_count) = &action else {
        panic!("wrong case");
    };
    assert!(std::ptr::eq(internal, direct));
    assert!(std::ptr::eq(count, direct_count));
    assert_eq!((internal.0.as_str(), *count), ("tuple", 1));
    let named = ActionPaths::named().embed((Internal(String::from("named")), 2));
    assert_eq!(ActionPaths::named().extract(&named).unwrap().0.0, "named");
    assert!(path.extract(&Action::Idle).is_none());
    assert!(ActionPaths::named().extract(&Action::Idle).is_none());
    let idle = ActionPaths::idle();
    assert!(matches!(idle.embed(()), Action::Idle));
    assert_eq!(idle.extract(&Action::Idle), Some(()));
    assert!(idle.extract(&action).is_none());

    let path = StatePaths::pair();
    let mut state = path.embed((Internal(String::from("tuple")), 3));
    let (internal, count) = path.get_mut(&mut state).unwrap();
    internal.0.push_str(" changed");
    *count = 4;
    assert_eq!(
        path.get(&state)
            .map(|(internal, count)| (internal.0.as_str(), *count)),
        Some(("tuple changed", 4))
    );
    let mut named = StatePaths::named().embed((Internal(String::from("named")), 5));
    let (internal, count) = StatePaths::named().get_mut(&mut named).unwrap();
    internal.0.push_str(" changed");
    *count = 6;
    assert_eq!(
        StatePaths::named()
            .get(&named)
            .map(|(internal, count)| (internal.0.as_str(), *count)),
        Some(("named changed", 6))
    );
    assert!(path.get(&State::Idle).is_none());
    let mut idle = State::Idle;
    assert!(path.get_mut(&mut idle).is_none());
    assert!(StatePaths::named().get(&idle).is_none());
    assert!(StatePaths::named().get_mut(&mut idle).is_none());
    let path = StatePaths::idle();
    assert!(matches!(path.embed(()), State::Idle));
    assert_eq!(path.get(&idle), Some(()));
    assert_eq!(path.get_mut(&mut idle), Some(()));
    assert!(path.get(&state).is_none());
    assert!(path.get_mut(&mut state).is_none());
    assert_eq!(format!("{ActionPaths:?}"), "ActionPaths");
    assert_eq!(format!("{StatePaths:?}"), "StatePaths");
    assert_debug_error(&ActionPaths);
    assert_debug_error(&StatePaths);
}
