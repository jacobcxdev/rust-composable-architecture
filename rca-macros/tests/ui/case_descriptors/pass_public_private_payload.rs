use rca_macros::CaseDescriptors;

struct Internal(u8);
#[derive(CaseDescriptors)]
pub enum Event {
    Pair(Internal, usize),
    Named { value: Internal, count: usize },
    Idle,
}

fn main() {
    let event = Event::Pair(Internal(3), 4);
    let EventCaseRef::Pair(value, count) = EventCases::project(&event) else {
        panic!("wrong case")
    };
    assert_eq!((value.0, *count), (3, 4));
}
