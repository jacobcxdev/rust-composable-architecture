use rca_macros::StatePaths;

#[derive(StatePaths)]
struct State {
    #[rca(optional)]
    child: u8,
}

fn main() {}
