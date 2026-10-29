use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[cfg(any())]
    Pair(Missing, Missing),
    Unit,
}

fn main() {
    let _ = ActionPaths::pair();
}
