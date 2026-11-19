use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    #[rca(optional)]
    Child(u8),
}

fn main() {}
