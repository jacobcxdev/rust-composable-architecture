use rca_macros::StatePaths;

#[derive(StatePaths)]
union UnionState {
    value: u8,
}

fn main() {}
