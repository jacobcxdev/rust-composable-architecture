use rca_core::Snapshot;

fn capture<T: Snapshot>(source: &T) -> T::Value {
    source.snapshot()
}

fn main() {
    let value = 3_u8;
    let _ = capture(&&value);
}
