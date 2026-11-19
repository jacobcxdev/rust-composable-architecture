// Rejects distinct case names that become the same path constructor.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    ChildState(u8),
    #[expect(
        non_camel_case_types,
        reason = "the fixture verifies transformed method collision detection"
    )]
    Child_State(u8),
}

fn main() {}
