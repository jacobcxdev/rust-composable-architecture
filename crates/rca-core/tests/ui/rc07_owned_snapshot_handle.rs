use std::cell::Cell;
use std::rc::Rc;

use rca_core::Snapshot;

fn capture<T: Snapshot>(source: &T) -> T::Value {
    source.snapshot()
}

fn main() {
    let _ = capture(&Rc::new(Cell::new(3)));
}
