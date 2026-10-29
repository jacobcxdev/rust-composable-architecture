use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1, live = 2)]
struct DuplicateLive;

fn main() {}
