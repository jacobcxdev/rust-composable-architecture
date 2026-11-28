use rca_macros::DependencyClient;
use std::sync::Arc;

#[derive(DependencyClient)]
struct Ambiguous {
    #[dependency(test = Arc::new(|| ()), member = Arc::new(|| ()))]
    call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyClient)]
struct ReverseAmbiguous {
    #[dependency(member = 1, test = 2)]
    value: usize,
}

#[derive(DependencyClient)]
struct Duplicate {
    #[dependency(member = 1, member = 2)]
    value: usize,
}

#[derive(DependencyClient)]
struct ImplicitMember {
    value: usize,
}

#[derive(DependencyClient)]
struct TestIsNotMember {
    #[dependency(test = 1)]
    value: usize,
}

#[derive(DependencyClient)]
struct WrongMemberType {
    #[dependency(member = "wrong")]
    value: usize,
}

fn main() {}
