use rca_macros::ActionPaths;

#[derive(ActionPaths)]
#[repr(u8)]
enum Action {
    Pair(u8, u16) = 1,
    Named { first: u8, second: u16 } = 2,
    Child(u8) = 3,
    Idle = 4,
}

fn main() {
    let path = ActionPaths::pair();
    let action = path.embed((1, 2));
    assert_eq!(path.extract(&action), Some((&1, &2)));
    let named = ActionPaths::named().embed((3, 4));
    assert_eq!(ActionPaths::named().extract(&named), Some((&3, &4)));
    assert_eq!(ActionPaths::child().extract(&Action::Child(5)), Some(&5));
    assert_eq!(ActionPaths::idle().extract(&Action::Idle), Some(()));
}
