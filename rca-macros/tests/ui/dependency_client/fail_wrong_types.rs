use rca_macros::DependencyClient;
use std::sync::Arc;

#[derive(DependencyClient)]
struct Client {
    call: Arc<dyn Fn(String) -> usize + Send + Sync>,
}
#[derive(DependencyClient)]
struct WrongDefault {
    #[dependency(test = Arc::new(|| "wrong"))]
    call: Arc<dyn Fn() -> usize + Send + Sync>,
}

fn main() {
    let _ = Client {
        call: Arc::new(|_: u8| 0),
        ..Client::test_value()
    };
}
