use rca_macros::CaseDescriptors;

#[derive(CaseDescriptors)]
enum Event {
    Value(String),
}

fn escaped() -> EventCaseRef<'static> {
    let event = Event::Value(String::from("local"));
    EventCases::project(&event)
}

fn main() {}
