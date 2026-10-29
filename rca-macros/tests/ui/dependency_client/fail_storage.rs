use rca_macros::DependencyClient;
use std::sync::Arc;

type Endpoint = Arc<dyn Fn() + Send + Sync>;
#[derive(DependencyClient)]
struct Alias {
    call: Endpoint,
}
#[derive(DependencyClient)]
struct Boxed {
    call: Box<dyn Fn() + Send + Sync>,
}
#[derive(DependencyClient)]
struct Concrete {
    call: Arc<u8>,
}
#[derive(DependencyClient)]
struct Mutable {
    call: Arc<dyn FnMut() + Send + Sync>,
}
#[derive(DependencyClient)]
struct Once {
    call: Arc<dyn FnOnce() + Send + Sync>,
}
#[derive(DependencyClient)]
struct NoSend {
    call: Arc<dyn Fn() + Sync>,
}
#[derive(DependencyClient)]
struct NoSync {
    call: Arc<dyn Fn() + Send>,
}

fn main() {}
