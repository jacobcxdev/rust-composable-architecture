// Rejects a lookalike payload that is not RCA's keyed action.

use rca_macros::ActionPaths;

struct WrongKeyed<Id, Child>(Id, Child);

#[derive(ActionPaths)]
enum Action {
    #[rca(keyed)]
    Row(WrongKeyed<u8, u8>),
}

fn main() {}
