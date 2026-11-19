#![deny(warnings)]

use rca_core::Task;

// Counterparts to compiler/fail/borrowed_read.rs and rc05_borrowed_capture.rs:
// keep the retained task and read operations, but capture and return owned values.
fn owned_task(value: String) -> Task<String, ()> {
    Task::<String, ()>::run(move |context| async move {
        let read = context.read(|state| state.clone()).await.unwrap();
        std::hint::black_box((value, read));
    })
}

fn main() {
    // Constructing and dropping the task needs no runtime or external service.
    drop(owned_task(String::from("owned input")));
}
