#[rca_macros::reducer(
    state(empty),
    action(generated(enum Action {
        #[cfg(feature = "selected")]
        Value(u8),
        #[cfg(not(feature = "selected"))]
        Value(String),
    })),
    body(generated),
)]
struct Feature {}

#[rca_macros::reducer(
    state(empty), action(empty), body(generated),
    #[cfg(any())]
    child(absent, reducer = Absent, kind = required,
        state = missing_state(), action = missing_action(),
        bind(fn bind_absent(child: Absent))),
)]
struct OmittedChild {}

fn main() {
    let _: &dyn rca::Reducer<State = (), Action = core::convert::Infallible> = &OmittedChild {};
    let _: &dyn rca::Reducer<State = (), Action = Action> = &Feature {};
    #[cfg(feature = "selected")]
    let action = ActionPaths::value().embed(7);
    #[cfg(not(feature = "selected"))]
    let action = ActionPaths::value().embed(String::from("local"));
    let Action::Value(original) = &action;
    let projection = ActionPaths::value().extract(&action).unwrap();
    assert!(core::ptr::eq(projection, original));
    #[cfg(feature = "selected")]
    assert_eq!(*projection, 7);
    #[cfg(not(feature = "selected"))]
    assert_eq!(projection, "local");
}
