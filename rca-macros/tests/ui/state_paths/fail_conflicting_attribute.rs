use rca_macros::StatePaths;

#[derive(StatePaths)]
struct State {
    #[rca(required, optional)]
    child: Option<u8>,
}

fn main() {}
