#![deny(warnings)]

use rca::DependencyKey as _;
use rca_macros::DependencyKey;

struct Value(u8);

#[derive(DependencyKey)]
#[r#dependency(r#test = Value(N), r#live = Value(7), r#value = Value)]
struct r#Key<const N: u8>;

fn main() {
    assert_eq!(Key::<3>::live_value().0, 7);
    assert_eq!(Key::<3>::test_value().0, 3);
    assert_eq!(Key::<5>::test_value().0, 5);
}
