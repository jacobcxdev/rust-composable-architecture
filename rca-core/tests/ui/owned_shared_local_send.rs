use rca_core::Shared;
use std::rc::Rc;

fn main() {
    let shared = Shared::new(Rc::new(0));
    std::thread::spawn(move || {
        drop(shared);
    });
}
