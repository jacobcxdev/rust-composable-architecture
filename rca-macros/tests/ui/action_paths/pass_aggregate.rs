#![deny(warnings)]
#![expect(
    dead_code,
    non_upper_case_globals,
    reason = "unused payloads and lowercase constants exercise generated-name collisions"
)]

use rca_macros::ActionPaths;

const __rca_root: usize = 1;
const __rca_payload: usize = 1;
const __rca_field_0: usize = 1;

pub struct MoveOnly(String);

#[derive(ActionPaths)]
pub enum Action<'a, T: AsRef<str>, const N: usize>
where
    Self: Sized,
{
    Unit,
    Pair(T, &'a str, [u8; N]),
    Named {
        first: T,
        second: MoveOnly,
    },
    Recursive {
        next: Box<Self>,
        count: usize,
    },
    #[cfg_attr(all(), cfg(any()))]
    Disabled(Missing, Missing),
}

#[derive(ActionPaths)]
enum Code {
    First = 1,
    Second = 2,
}

fn main() {
    let local = String::from("local");
    let path = ActionPaths::<String, 1>::pair();
    let action = path.embed((String::from("owned"), local.as_str(), [1]));
    let (owned, label, bytes) = path.extract(&action).unwrap();
    assert_eq!((owned.as_str(), *label, bytes), ("owned", "local", &[1]));
    let named = ActionPaths::<String, 1>::named().embed((String::new(), MoveOnly(String::new())));
    let _ = ActionPaths::<String, 1>::named().extract(&named);
    let _ = CodePaths::first().embed(());
}
