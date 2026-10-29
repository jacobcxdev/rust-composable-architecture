//! Keeps the umbrella's named-operation surface usable without state sharing.

use rca::{
    CancellingOperationWait, Effect, Operation, OperationPolicy, OperationStatus, OperationWait,
    SelfWait, Task, TaskAccessError,
};
use std::cell::Cell;

/// Checks owned request bounds without polling the request or starting an actor.
fn owned_send<T>(_: impl Future<Output = T> + Send + 'static) {}

#[test]
fn facade_exposes_named_declarations_and_owned_request_types() {
    fn handle<T: Clone + Send + Sync + 'static>() {}
    fn wait<T: Future<Output = Result<(), SelfWait>> + Send + 'static>() {}
    handle::<Operation>();
    handle::<OperationStatus>();
    wait::<OperationWait>();
    wait::<CancellingOperationWait>();

    let effect = Effect::<()>::none().named("background", OperationPolicy::Coexist);
    let task = Task::<Cell<usize>, ()>::run_named(
        "background",
        OperationPolicy::Replace,
        |context| async move {
            let operation: Operation = context.operation("background");
            owned_send::<Result<OperationStatus, TaskAccessError>>(operation.status());
            owned_send::<Result<OperationWait, TaskAccessError>>(operation.capture());
            owned_send::<Result<(), TaskAccessError>>(operation.cancel());
        },
    );
    drop((effect, task));
}
