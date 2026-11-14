use rca::Effect;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn map_non_send_source(effect: Effect<Rc<usize>>) -> Effect<usize> {
    effect.map(|value| *value)
}

fn main() {
    drop(map_non_send_source(Effect::none()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mapped_calls = Arc::clone(&calls);
    let mapped: Effect<Rc<usize>> = Effect::send(7usize).map(move |value| {
        mapped_calls.fetch_add(1, Ordering::SeqCst);
        Rc::new(value)
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(mapped);
}
