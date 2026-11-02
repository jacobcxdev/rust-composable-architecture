use rca_core::TaskContext;
use std::rc::Rc;
fn wrong(context: TaskContext<usize, ()>) {
    let _ = context.update(|state| Rc::new(*state));
}
fn main() {}
