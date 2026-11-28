use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    #[rca(keyed)]
    Unit,
    Multiple(u8, u16),
}

fn main() {}
