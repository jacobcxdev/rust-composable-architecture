use std::cell::Cell;
use std::rc::Rc;

use rca_core::local::{Operation, Store, TaskContext};

fn owned(_: impl Future + 'static) {}
fn send(_: impl Send + 'static) {}
fn portable<T: Send + Sync>() {}

fn local(
    store: Store<Rc<Cell<usize>>, Rc<usize>>,
    context: TaskContext<Rc<Cell<usize>>, Rc<usize>>,
) {
    let root: Operation = store.operation("work");
    let scoped: Operation = context.operation("work");
    owned(root.status());
    owned(scoped.capture());
    owned(scoped.cancel());
    drop((root, scoped, store, context));
}

fn shared(operation: rca_core::Operation) {
    send(operation.status());
    send(operation.capture());
    send(operation.cancel());
    send(operation);
    portable::<rca_core::OperationWait>();
}

fn main() {}
