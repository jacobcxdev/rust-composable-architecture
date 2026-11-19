use rca_macros::ActionPaths;

mod domain {
    use super::ActionPaths;

    #[derive(ActionPaths)]
    enum Action {
        Pair(String, usize),
    }
}

fn main() {
    let _ = domain::ActionPaths::pair();
}
