#![deny(warnings)]
#![expect(
    dead_code,
    non_upper_case_globals,
    reason = "unused payloads and lowercase constants exercise generated-name collisions"
)]

use rca_macros::StatePaths;

const __rca_root: usize = 1;
const __rca_payload: usize = 1;
const __rca_field_0: usize = 1;

pub struct MoveOnly(String);

#[derive(StatePaths)]
pub enum State<'a, T: AsRef<str>, const N: usize>
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

#[derive(StatePaths)]
enum Code {
    First = 1,
    Second = 2,
}

fn main() {
    let local = String::from("local");
    let path = StatePaths::<String, 1>::pair();
    let mut state = path.embed((String::from("owned"), local.as_str(), [1]));
    let (owned, label, bytes) = path.get_mut(&mut state).unwrap();
    owned.push_str(" changed");
    *label = "new";
    bytes[0] = 2;
    let _ = StatePaths::<String, 1>::recursive();
    let _ = StatePaths::<String, 1>::named().embed((String::new(), MoveOnly(String::new())));
    let _ = CodePaths::first().embed(());
}
