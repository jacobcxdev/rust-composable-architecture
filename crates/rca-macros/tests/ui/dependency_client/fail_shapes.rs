use rca_macros::DependencyClient;

#[derive(DependencyClient)]
enum Enum {
    A,
}
#[derive(DependencyClient)]
union Union {
    a: u8,
}
#[derive(DependencyClient)]
struct Tuple(u8);
#[derive(DependencyClient)]
struct Unit;

fn main() {}
