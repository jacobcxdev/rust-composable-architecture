use rca_macros::CaseDescriptors;

#[derive(CaseDescriptors)]
enum Event {
    ALL,
    Idle,
}

fn main() {
    let catalogue: &[EventCase] = EventCases::ALL;
    assert_eq!(catalogue, &[EventCase::ALL, EventCase::Idle]);
    assert_eq!(EventCases::case(&Event::ALL), EventCase::ALL);
}
