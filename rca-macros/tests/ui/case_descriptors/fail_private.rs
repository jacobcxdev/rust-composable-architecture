use rca_macros::CaseDescriptors;

mod domain {
    use super::CaseDescriptors;
    #[derive(CaseDescriptors)]
    enum Private {
        Idle,
    }
}

fn main() {
    let _ = domain::PrivateCases::ALL;
}
