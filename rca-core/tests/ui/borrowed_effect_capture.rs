// Retained effect work cannot capture a caller's temporary borrow.

use rca_core::Effect;

fn borrowed_effect(value: &str) -> Effect<()> {
    Effect::run(move |_sender| async move {
        std::hint::black_box(value);
    })
}

fn main() {
    let value = String::from("borrowed");
    let _effect = borrowed_effect(&value);
}
