use rca_core::{Task, TaskContext};
use std::cell::Cell;
struct State(Cell<usize>);
struct Action(Cell<usize>);
fn send_sync<T: Send + Sync>() {}
fn send<T: Send>(_: T) {}
fn main() {
    send_sync::<TaskContext<State, Action>>();
    send(Task::<State, Action>::run(|context| async move {
        let _ = context.send(Action(Cell::new(1))).await;
        let _ = context.read(|state| Cell::new(state.0.get())).await;
        let _ = context
            .update(|state| {
                state.0.set(3);
                Cell::new(3)
            })
            .await;
    }));
}
