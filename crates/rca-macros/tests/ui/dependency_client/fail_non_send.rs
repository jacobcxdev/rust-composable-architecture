use rca_macros::DependencyClient;
use std::sync::{Arc, Mutex};

#[derive(DependencyClient)]
struct Client {
    call: Arc<dyn Fn() -> usize + Send + Sync>,
}

fn main() {
    static VALUE: Mutex<usize> = Mutex::new(3);
    let value = VALUE.lock().unwrap();
    let _ = Client {
        call: Arc::new(move || *value),
        ..Client::test_value()
    };
}
