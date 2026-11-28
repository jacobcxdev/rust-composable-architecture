use rca_macros::StatePaths;

#[derive(StatePaths)]
struct State {
    #[rca(keyed)]
    child: Vec<u8>,
}

fn main() {}
