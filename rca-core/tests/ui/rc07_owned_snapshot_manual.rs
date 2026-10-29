use std::cell::Cell;
use std::rc::Rc;

use rca_core::Snapshot;

struct Local<'a>(&'a Cell<usize>);
// No Clone, Debug, PartialEq, Send, Sync or Default.
struct Value(Rc<usize>);
impl Snapshot for Local<'_> {
    type Value = Value;
    fn snapshot(&self) -> Value {
        Value(Rc::new(self.0.get()))
    }
}

fn main() {
    let captured = {
        let local = Cell::new(3);
        Local(&local).snapshot()
    };
    assert_eq!(*captured.0, 3);
}
