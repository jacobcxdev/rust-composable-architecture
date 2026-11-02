use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1, preview = 2)]
struct UnknownOption;

fn main() {}
