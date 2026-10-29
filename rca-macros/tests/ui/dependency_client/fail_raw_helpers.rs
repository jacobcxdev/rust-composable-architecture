use std::sync::Arc;

use rca_macros::DependencyClient;

#[derive(DependencyClient)]
#[r#dependency(r#test = ())]
struct Misplaced {
    call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyClient)]
struct DuplicateHelpers {
    #[dependency(test = Arc::new(|| ()))]
    #[r#dependency(r#test = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyClient)]
struct DuplicateTest {
    #[dependency(test = Arc::new(|| ()), r#test = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyClient)]
struct DuplicateMember {
    #[r#dependency(member = 1, r#member = 2)]
    value: usize,
}

#[derive(DependencyClient)]
struct Exclusive {
    #[dependency(r#member = Arc::new(|| ()), r#test = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyClient)]
struct Unknown {
    #[r#dependency(r#live = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyClient)]
struct Qualified {
    #[dependency(r#nested::r#test = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}

fn main() {}
