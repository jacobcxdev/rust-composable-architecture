use rca_macros::StatePaths;

#[derive(StatePaths)]
#[repr(u8)]
enum State {
    Pair(u8, u16) = 1,
    Named { first: u8, second: u16 } = 2,
    Child(u8) = 3,
    Idle = 4,
}

fn main() {
    let path = StatePaths::pair();
    let mut state = path.embed((1, 2));
    let (first, second) = path.get_mut(&mut state).unwrap();
    *first = 3;
    *second = 4;
    assert_eq!(path.get(&state), Some((&3, &4)));
    let named = StatePaths::named().embed((5, 6));
    assert_eq!(StatePaths::named().get(&named), Some((&5, &6)));
    assert_eq!(StatePaths::child().get(&State::Child(7)), Some(&7));
    assert_eq!(StatePaths::idle().get(&State::Idle), Some(()));
}
