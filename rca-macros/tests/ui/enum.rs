use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1)]
enum EnumKey {
    Value,
}

fn main() {}
