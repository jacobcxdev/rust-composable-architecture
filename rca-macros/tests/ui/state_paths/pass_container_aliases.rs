use rca::{KeyedState, KeyedStatePath, OptionalStatePath};
use rca_macros::StatePaths;
use std::{cell::Cell, rc::Rc};

type Maybe<T> = Option<T>;
type Rows<Id, T> = KeyedState<Id, T>;
type Detail = Maybe<String>;
type ConcreteRows = Rows<u8, String>;

// Deliberately neither Clone nor Debug, and neither Send nor Sync.
struct BorrowedMoveOnly<'a> {
    value: &'a Cell<u8>,
    local: Rc<Cell<u8>>,
}

#[derive(StatePaths)]
struct BorrowedState<'a> {
    #[rca(optional)]
    optional: Maybe<BorrowedMoveOnly<'a>>,
    #[rca(keyed)]
    keyed: Rows<u8, BorrowedMoveOnly<'a>>,
}

#[derive(StatePaths)]
struct GenericPayload<Optional, Keyed> {
    #[rca(optional)]
    optional: Optional,
    #[rca(keyed)]
    keyed: Keyed,
}

#[derive(StatePaths)]
struct State<T> {
    #[rca(optional)]
    generic: Maybe<T>,
    #[rca(optional)]
    concrete: Detail,
    #[rca(keyed)]
    rows: Rows<u8, T>,
    #[rca(keyed)]
    concrete_rows: ConcreteRows,
}

fn main() {
    let optional_value = Cell::new(1);
    let keyed_value = Cell::new(2);
    let mut borrowed = BorrowedState {
        optional: Some(BorrowedMoveOnly {
            value: &optional_value,
            local: Rc::new(Cell::new(3)),
        }),
        keyed: [(
            1,
            BorrowedMoveOnly {
                value: &keyed_value,
                local: Rc::new(Cell::new(4)),
            },
        )]
        .into_iter()
        .collect(),
    };
    let optional = BorrowedStatePaths::optional();
    optional.get_mut(&mut borrowed).unwrap().value.set(5);
    assert_eq!(optional.get(&borrowed).unwrap().value.get(), 5);
    assert_eq!(optional.get(&borrowed).unwrap().local.get(), 3);
    let keyed = BorrowedStatePaths::keyed();
    keyed
        .get_mut(&mut borrowed)
        .get_mut(&1)
        .unwrap()
        .value
        .set(6);
    assert_eq!(keyed.get(&borrowed).get(&1).unwrap().value.get(), 6);
    assert_eq!(keyed.get(&borrowed).get(&1).unwrap().local.get(), 4);
    let generic = GenericPayload {
        optional: Some(3_u8),
        keyed: [(1_u8, 4_u8)].into_iter().collect::<KeyedState<_, _>>(),
    };
    assert_eq!(
        GenericPayloadPaths::<Option<u8>, KeyedState<u8, u8>>::optional().get(&generic),
        Some(&3)
    );
    assert_eq!(
        GenericPayloadPaths::<Option<u8>, KeyedState<u8, u8>>::keyed()
            .get(&generic)
            .get(&1),
        Some(&4)
    );
    let mut state = State {
        generic: Some(String::from("generic")),
        concrete: Some(String::from("concrete")),
        rows: [(1, String::from("row"))].into_iter().collect(),
        concrete_rows: [(2, String::from("concrete row"))].into_iter().collect(),
    };
    let manual_optional = OptionalStatePath::new(
        |state: &State<String>| state.generic.as_ref(),
        |state: &mut State<String>| state.generic.as_mut(),
    );
    let generated_optional = StatePaths::<String>::generic();
    assert_eq!(generated_optional.get(&state), manual_optional.get(&state));
    generated_optional.get_mut(&mut state).unwrap().push('!');
    assert_eq!(state.generic.as_deref(), Some("generic!"));
    assert_eq!(
        StatePaths::<String>::concrete().get(&state).unwrap(),
        "concrete"
    );
    let manual_keyed = KeyedStatePath::new(
        |state: &State<String>| &state.rows,
        |state: &mut State<String>| &mut state.rows,
    );
    let generated_keyed = StatePaths::<String>::rows();
    assert_eq!(generated_keyed.get(&state), manual_keyed.get(&state));
    generated_keyed
        .get_mut(&mut state)
        .get_mut(&1)
        .unwrap()
        .push('!');
    assert_eq!(state.rows.get(&1).unwrap(), "row!");
    assert_eq!(
        StatePaths::<String>::concrete_rows()
            .get(&state)
            .get(&2)
            .unwrap(),
        "concrete row"
    );
}
