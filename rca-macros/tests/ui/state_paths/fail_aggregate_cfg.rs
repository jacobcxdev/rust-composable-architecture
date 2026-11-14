use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    #[cfg(any())]
    Pair(Missing, Missing),
    Unit,
}

fn main() {
    let _ = StatePaths::pair();
}
