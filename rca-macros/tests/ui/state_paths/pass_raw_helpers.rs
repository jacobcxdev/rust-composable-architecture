#![deny(warnings)]

use std::cell::Cell;
use std::rc::Rc;

use rca::{CaseStatePath, KeyedState, KeyedStatePath, OptionalStatePath, StatePath};
use rca_macros::StatePaths;

struct Payload<'a> {
    text: &'a str,
    local: Rc<Cell<u8>>,
}

#[derive(StatePaths)]
struct r#State<'a> {
    #[r#rca(r#required)]
    required: Payload<'a>,
    #[r#rca(r#optional)]
    optional: Option<Payload<'a>>,
    #[r#rca(r#keyed)]
    keyed: KeyedState<u8, Payload<'a>>,
    #[r#rca(r#skip)]
    skipped: u8,
}

impl StatePaths<'_> {
    fn skipped() -> u8 {
        7
    }
}

#[derive(StatePaths)]
enum r#Choice<'a> {
    r#Present(Payload<'a>),
    #[r#rca(r#skip)]
    r#Absent,
}

impl ChoicePaths<'_> {
    fn absent() -> Choice<'static> {
        Choice::Absent
    }
}

fn payload(text: &str) -> Payload<'_> {
    Payload {
        text,
        local: Rc::new(Cell::new(3)),
    }
}

fn main() {
    let original = String::from("original");
    let replacement = String::from("replacement");
    let mut state = r#State {
        required: payload(&original),
        optional: Some(payload(&original)),
        keyed: [(1, payload(&original))].into_iter().collect(),
        skipped: 7,
    };
    let required: StatePath<State<'_>, Payload<'_>, _, _> = StatePaths::required();
    assert!(std::ptr::eq(required.get(&state), &state.required));
    required.get_mut(&mut state).text = &replacement;
    assert_eq!(state.required.text, "replacement");
    assert_eq!(state.required.local.get(), 3);

    let optional: OptionalStatePath<State<'_>, Payload<'_>, _, _> = StatePaths::optional();
    assert!(std::ptr::eq(
        optional.get(&state).unwrap(),
        state.optional.as_ref().unwrap(),
    ));
    optional.get_mut(&mut state).unwrap().text = &replacement;
    assert_eq!(state.optional.as_ref().unwrap().text, "replacement");
    state.optional = None;
    assert!(optional.get(&state).is_none());
    assert!(optional.get_mut(&mut state).is_none());

    let keyed: KeyedStatePath<State<'_>, u8, Payload<'_>, _, _> = StatePaths::keyed();
    assert!(std::ptr::eq(keyed.get(&state), &state.keyed));
    keyed.get_mut(&mut state).get_mut(&1).unwrap().text = &replacement;
    assert_eq!(state.keyed.get(&1).unwrap().text, "replacement");
    assert_eq!(StatePaths::skipped(), state.skipped);

    let mut choice = r#Choice::r#Present(payload(&original));
    let present: CaseStatePath<Choice<'_>, Payload<'_>, _, _> = ChoicePaths::present();
    let Choice::Present(value) = &choice else {
        panic!("present fixture value");
    };
    assert!(std::ptr::eq(present.get(&choice).unwrap(), value));
    present.get_mut(&mut choice).unwrap().text = &replacement;
    let Choice::Present(value) = &choice else {
        panic!("projection must not change the variant");
    };
    assert_eq!(value.text, "replacement");
    choice = ChoicePaths::absent();
    assert!(present.get(&choice).is_none());
    assert!(present.get_mut(&mut choice).is_none());
}
