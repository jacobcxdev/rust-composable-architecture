mod domain {
    use rca_macros::DependencyKey;
    #[derive(DependencyKey)]
    #[dependency(value = usize, live = 7)]
    struct Key;
}
fn main() {
    let _ = <domain::Key as rca::DependencyKey>::live_value();
}
