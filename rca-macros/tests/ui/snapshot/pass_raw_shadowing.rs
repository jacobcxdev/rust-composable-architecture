#![deny(warnings)]

use rca::Snapshot as _;
use rca_macros::Snapshot;

#[derive(Snapshot)]
struct r#Source<Source> {
    item: Source,
}

#[derive(Snapshot)]
struct Other<r#Other> {
    item: Other,
}

trait Projection {
    type Value;
    fn capture(&self) -> Self::Value;
}

struct Payload(u8);
impl Projection for Payload {
    type Value = u8;
    fn capture(&self) -> u8 {
        self.0
    }
}

#[derive(Snapshot)]
struct r#Projected<Projected: Projection> {
    #[snapshot(with = r#Projected::capture, value = Projected::Value)]
    item: r#Projected,
}

fn main() {
    assert_eq!(Source { item: 3u8 }.snapshot().item, 3);
    assert_eq!(Other { item: 4u8 }.snapshot().item, 4);
    assert_eq!(Projected { item: Payload(5) }.snapshot().item, 5);
}
