use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1, test = 2, test = 3)]
struct DuplicateTest;

fn main() {}
