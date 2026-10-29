use rca_core::Task;
use std::rc::Rc;
fn main() {
    let value = Rc::new(1);
    let _ = Task::<usize, ()>::run(move |_| async move {
        drop(value);
    });
}
