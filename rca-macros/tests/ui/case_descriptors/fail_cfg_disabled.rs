use rca_macros::CaseDescriptors;

#[derive(CaseDescriptors)]
enum Event {
    Idle,
    #[cfg(any())]
    Disabled,
}

fn main() {
    let _ = EventCase::Disabled;
}
