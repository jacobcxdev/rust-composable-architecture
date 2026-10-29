use rca_core::Store;
use std::cell::Cell;

struct State(Cell<Option<Box<usize>>>);
struct Action(Cell<Option<Box<usize>>>);

fn require_send_static<T: Send + 'static>(_: T) {}

fn check(store: Store<State, Action>) {
    require_send_static(store.shutdown_into_state());
}

fn observation_bounds<State: 'static, Action: Send + 'static>(store: Store<State, Action>) {
    require_send_static(async move {
        let _ = store.observe(|_| ()).await;
        let _ = store.observe_distinct(|_| ()).await;
    });
}

fn main() {
    let _ = check;
    let _ = observation_bounds::<std::rc::Rc<()>, ()>;
    let _ = State(Cell::new(None)).0.take();
    let _ = Action(Cell::new(None)).0.take();
}
