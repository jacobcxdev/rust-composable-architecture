use rca_macros::DependencyClient;
use std::sync::Arc;

#[derive(DependencyClient)]
#[dependency(test = ())]
struct TypeAttribute {
    call: Arc<dyn Fn() + Send + Sync>,
}
#[derive(DependencyClient)]
struct Duplicate {
    #[dependency(test = Arc::new(|| ()), test = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}
#[derive(DependencyClient)]
struct DuplicateAttribute {
    #[dependency(test = Arc::new(|| ()))]
    #[dependency(test = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}
#[derive(DependencyClient)]
struct Unknown {
    #[dependency(live = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}
#[derive(DependencyClient)]
struct Empty {
    #[dependency()]
    call: Arc<dyn Fn() + Send + Sync>,
}

fn main() {}
