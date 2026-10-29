use rca_macros::StatePaths;

#[derive(StatePaths)]
struct State {
    #[rca(optional)]
    #[rca(optional)]
    child: Option<u8>,
}

fn main() {}
