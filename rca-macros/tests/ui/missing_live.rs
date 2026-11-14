use rca_macros::DependencyKey;

#[derive(DependencyKey)]
#[dependency(value = u8)]
struct MissingLive;

fn main() {}
