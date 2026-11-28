use rca::Snapshot as _;
use rca_macros::Snapshot;

#[derive(Snapshot)]
struct Reading(f32);

fn require_eq<T: Eq>(_: &T) {}

fn main() {
    let value = Reading(0.0).snapshot();
    assert_eq!(value, value.clone());
    require_eq(&value);
}
