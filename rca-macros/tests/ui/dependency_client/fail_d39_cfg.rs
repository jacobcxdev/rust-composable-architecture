use rca_macros::DependencyClient;
#[derive(DependencyClient)]
struct Client {
    #[cfg(any())]
    #[dependency(member = 3)]
    disabled: usize,
}
fn main() {
    let _ = Client::test_value().disabled;
}
