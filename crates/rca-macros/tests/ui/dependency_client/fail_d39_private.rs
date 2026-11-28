mod domain {
    use rca_macros::DependencyClient;
    #[derive(DependencyClient)]
    struct Client {}
}

fn main() {
    let _ = domain::Client::test_value();
}
