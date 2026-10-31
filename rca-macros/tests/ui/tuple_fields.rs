use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8, live = 1)]
struct TupleFields(u8);

fn main() {}
