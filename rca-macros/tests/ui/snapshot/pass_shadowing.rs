#![deny(warnings)]

use rca::Snapshot;
use rca_macros::Snapshot;

#[derive(Snapshot)]
struct Source<Source> {
    item: Source,
}

#[derive(Snapshot)]
enum Event<Event> {
    Idle,
    Value(Event),
    Named { item: Event },
}

trait Projection {
    type Value;
    fn capture(&self) -> Self::Value;
}

struct Payload(usize);
impl Projection for Payload {
    type Value = usize;
    fn capture(&self) -> usize {
        self.0
    }
}

fn capture<T: Projection>(value: &T) -> T::Value {
    value.capture()
}

#[derive(Snapshot)]
struct Projected<Projected: Projection, __RcaSnapshotSource>
where
    <Projected as Projection>::Value: Snapshot,
{
    projected: Projected::Value,
    #[snapshot(with = Projected::capture, value = Projected::Value)]
    associated: Projected,
    #[snapshot(with = capture::<Projected>, value = <Projected as Projection>::Value)]
    generic: Projected,
    #[snapshot(omit = "The marker carries no historical state")]
    marker: std::marker::PhantomData<__RcaSnapshotSource>,
}

struct History<T> {
    value: T,
}
#[derive(Snapshot)]
#[snapshot(into = History<<Destination as Projection>::Value>)]
struct Destination<Destination: Projection> {
    #[snapshot(with = Destination::capture, value = Destination::Value)]
    value: Destination,
}

#[allow(
    non_snake_case,
    reason = "the regression distinguishes type and value namespaces"
)]
fn Bare<T>(_: &T) -> usize {
    19
}
#[derive(Snapshot)]
struct Bare<Bare> {
    #[snapshot(with = Bare, value = usize)]
    item: Bare,
}

fn main() {
    assert_eq!(Bare { item: Payload(0) }.snapshot().item, 19);
    let captured = Projected::<Payload, ()> {
        projected: 13,
        associated: Payload(14),
        generic: Payload(15),
        marker: std::marker::PhantomData,
    }
    .snapshot();
    assert_eq!(
        (captured.projected, captured.associated, captured.generic),
        (13, 14, 15)
    );
    assert_eq!(Destination { value: Payload(16) }.snapshot().value, 16);
    assert_eq!(Source { item: 7usize }.snapshot().item, 7);
    assert_eq!(Event::<usize>::Idle.snapshot(), EventSnapshot::Idle);
    assert_eq!(Event::Value(8usize).snapshot(), EventSnapshot::Value(8));
    assert_eq!(
        Event::Named { item: 9usize }.snapshot(),
        EventSnapshot::Named { item: 9 }
    );

    #[derive(Snapshot)]
    struct Local<Local> {
        item: Local,
    }
    assert_eq!(Local { item: 11usize }.snapshot().item, 11);
}
