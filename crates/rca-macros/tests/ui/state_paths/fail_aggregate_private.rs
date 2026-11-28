use rca_macros::StatePaths;

mod domain {
    use super::StatePaths;

    #[derive(StatePaths)]
    enum State {
        Pair(String, usize),
    }
}

fn main() {
    let _ = domain::StatePaths::pair();
}
