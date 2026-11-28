use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, value = u16, live = 1)]
struct DuplicateValue;

fn main() {}
