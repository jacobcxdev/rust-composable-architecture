//! Exercises all derive families with renamed dependencies and inactive unknown types.

use r#gen::DependencyKey as _;
use std::sync::Arc;
use syntax::{ActionPaths, CaseDescriptors, DependencyClient, DependencyKey, StatePaths};

#[derive(DependencyClient)]
struct Client<const N: usize> {
    #[cfg(any())]
    disabled: MissingType,
    #[dependency(member = [5; N])]
    bytes: [u8; N],
    #[dependency(test = Arc::new(|value| value))]
    r#type: Arc<dyn Fn(String) -> String + Send + Sync>,
}

#[derive(DependencyKey)]
#[dependency(value = Client<N>, live = Client::test_value(), test = Client::test_value())]
struct Key<const N: usize>;

#[derive(StatePaths)]
struct State {
    value: usize,
    #[cfg(any())]
    disabled: MissingType,
}

#[derive(ActionPaths, CaseDescriptors)]
enum Event {
    Value(usize),
    Idle,
    #[cfg(any())]
    Disabled(MissingType),
}

fn main() {
    let client = Key::<2>::test_value();
    assert_eq!(client.bytes, [5, 5]);
    assert_eq!((client.r#type)(String::from("owned")), "owned");
    let state = State { value: 7 };
    assert_eq!(*StatePaths::value().get(&state), 7);
    let event = EventPaths::value().embed(9);
    assert_eq!(EventCases::case(&event), EventCase::Value);
    assert!(matches!(
        EventCases::project(&event),
        EventCaseRef::Value(&9)
    ));
    assert_eq!(EventCases::ALL, &[EventCase::Value, EventCase::Idle]);
    let _ = Event::Idle;
    let _: r#gen::Combine<_> = syntax::reducers![()];
}
