#![expect(dead_code, reason = "caller names deliberately shadow prelude names")]
use rca_macros::CaseDescriptors;

struct Option;
struct Some;
trait FnOnce {}

#[derive(CaseDescriptors)]
enum Event {
    Idle,
    Value(u8),
}

fn main() {
    assert_eq!(
        EventCase::Value.label(),
        ::core::option::Option::Some("Value")
    );
    assert_eq!(
        EventCase::Idle.label_with(|_| ::core::option::Option::None),
        ::core::option::Option::None
    );
    assert!(matches!(
        EventCases::project(&Event::Value(3)),
        EventCaseRef::Value(&3)
    ));
}
