use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1)]
union UnionKey {
    value: u8,
}

fn main() {}
