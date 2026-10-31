use rca_core::Snapshot;

#[derive(Clone)]
struct MerelyClone;

fn capture<T: Snapshot>(source: &T) -> T::Value {
    source.snapshot()
}

fn main() {
    let _ = capture(&MerelyClone);
}
