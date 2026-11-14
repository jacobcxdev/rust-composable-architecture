#![expect(
    non_upper_case_globals,
    reason = "constants reproduce generated binding names"
)]
use rca_macros::CaseDescriptors;

const __rca_source: u8 = 1;
const __rca_formatter: u8 = 2;
const __rca_label: u8 = 3;
const __rca_field_0: u8 = 4;

#[derive(CaseDescriptors)]
enum Event {
    Pair(String, u8),
    Named { text: String, count: u8 },
    Idle,
}

fn main() {
    let root = Event::Pair(String::from("local"), 7);
    let EventCaseRef::Pair(text, count) = EventCases::project(&root) else {
        panic!("wrong case")
    };
    assert_eq!((text.as_str(), *count), ("local", 7));
    assert_eq!(EventCases::case(&root), EventCase::Pair);
    assert_eq!(
        format!("{:?}", EventCases::project(&root)),
        "EventCaseRef::Pair"
    );
    assert_eq!(
        EventCase::Pair.label_with(|_| Some("custom")),
        Some("custom")
    );
    let named = Event::Named {
        text: String::from("named"),
        count: 8,
    };
    let EventCaseRef::Named { text, count } = EventCases::project(&named) else {
        panic!("wrong named case")
    };
    assert_eq!((text.as_str(), *count), ("named", 8));
    assert_eq!(EventCases::case(&Event::Idle), EventCase::Idle);
}
