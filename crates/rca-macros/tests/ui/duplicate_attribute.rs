use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1)]
#[dependency(value = u8, live = 2)]
struct DuplicateAttribute;

fn main() {}
