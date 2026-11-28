// Keeps a private state field's generated constructor private.

use rca_macros::StatePaths;

mod feature {
    use super::StatePaths;

    pub struct Child;

    #[derive(StatePaths)]
    pub struct State {
        child: Child,
    }
}

fn main() {
    let _ = feature::StatePaths::child();
}
