use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(live = 1)]
struct MissingValue;

fn main() {}
