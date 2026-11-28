use rca_macros::Snapshot;
mod private {
    pub struct Destination {
        hidden: usize,
    }
    pub fn read(value: Destination) -> usize {
        value.hidden
    }
}
#[derive(Snapshot)]
#[snapshot(into = private::Destination)]
struct Private {
    hidden: usize,
}
fn main() {
    use rca::Snapshot as _;
    let _ = private::read(Private { hidden: 1 }.snapshot());
}
