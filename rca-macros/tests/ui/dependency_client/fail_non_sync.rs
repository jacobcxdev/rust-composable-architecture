use rca_macros::DependencyClient;
use std::{cell::Cell, sync::Arc};

#[derive(DependencyClient)]
struct Client {
    call: Arc<dyn Fn() -> usize + Send + Sync>,
}

fn main() {
    let value = Cell::new(3);
    let _ = Client {
        call: Arc::new(move || value.get()),
        ..Client::test_value()
    };
}
