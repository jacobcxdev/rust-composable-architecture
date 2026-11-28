//! Checks generated path fallbacks when conditional or skipped variants affect matching.

#[test]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the reducer macro requires named storage, even without stored children"
)]
fn active_skipped_variants_retain_state_and_keyed_action_mismatches() {
    #[rca_macros::reducer(
        state(generated(enum State {
            Ready { text: String, revision: usize },
            #[cfg(all())]
            #[rca(skip)]
            Other,
        })),
        action(generated(enum Action {
            #[rca(keyed)]
            Ready(rca::KeyedAction<u8, String>),
            #[cfg(all())]
            #[rca(skip)]
            Other,
        })),
        body(generated),
    )]
    struct Feature {}

    let _: &dyn rca::Reducer<State = State, Action = Action> = &Feature {};

    let state_path = StatePaths::ready();
    let mut other = State::Other;
    assert!(state_path.get(&other).is_none());
    assert!(state_path.get_mut(&mut other).is_none());

    let mut state = state_path.embed((String::from("state"), 1));
    let (text, revision) = state_path.get_mut(&mut state).unwrap();
    text.push('!');
    *revision = 2;
    let (text, revision) = state_path.get(&state).unwrap();
    assert_eq!((text.as_str(), *revision), ("state!", 2));

    let action_path = ActionPaths::ready();
    assert!(action_path.extract(&Action::Other).is_none());
    let action = action_path.embed(7, String::from("action"));
    let (id, text) = action_path.extract(&action).unwrap();
    assert_eq!((*id, text.as_str()), (7, "action"));
}

#[test]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the reducer macro requires named storage, even without stored children"
)]
fn nested_cfg_disabled_alternatives_preserve_original_borrows() {
    #[rca_macros::reducer(
        state(generated(enum State {
            Ready(String),
            #[cfg_attr(all(), cfg_attr(all(), cfg(any())))]
            Other,
        })),
        action(generated(enum Action {
            Ready { text: String, revision: usize },
            #[cfg_attr(all(), cfg_attr(all(), cfg(any())))]
            Other,
        })),
        body(generated),
    )]
    struct Feature {}

    let _: &dyn rca::Reducer<State = State, Action = Action> = &Feature {};

    let state_path = StatePaths::ready();
    let mut state = State::Ready(String::from("state"));
    state_path.get_mut(&mut state).unwrap().push('!');
    let State::Ready(original) = &state;
    let borrowed = state_path.get(&state).unwrap();
    assert!(std::ptr::eq(borrowed, original));
    assert_eq!(borrowed, "state!");

    let action_path = ActionPaths::ready();
    let action = action_path.embed((String::from("action"), 3));
    let Action::Ready {
        text: original_text,
        revision: original_revision,
    } = &action;
    let (text, revision) = action_path.extract(&action).unwrap();
    assert!(std::ptr::eq(text, original_text));
    assert!(std::ptr::eq(revision, original_revision));
    assert_eq!((text.as_str(), *revision), ("action", 3));
}

#[test]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the reducer macro requires named storage, even without stored children"
)]
fn mutually_exclusive_duplicate_names_select_the_active_payload() {
    #[rca_macros::reducer(
        state(generated(enum State {
            #[cfg(any())]
            Ready(u8),
            #[cfg(all())]
            Ready(String),
        })),
        action(generated(enum Action {
            #[cfg(any())]
            Ready(u16),
            #[cfg(all())]
            Ready(String),
        })),
        body(generated),
    )]
    struct Feature {}

    let _: &dyn rca::Reducer<State = State, Action = Action> = &Feature {};

    let state_path = StatePaths::ready();
    let mut state = State::Ready(String::from("selected"));
    state_path.get_mut(&mut state).unwrap().push('!');
    let State::Ready(original) = &state;
    let borrowed = state_path.get(&state).unwrap();
    assert!(std::ptr::eq(borrowed, original));
    assert_eq!(borrowed, "selected!");

    let action_path = ActionPaths::ready();
    let action = action_path.embed(String::from("selected"));
    let Action::Ready(original) = &action;
    let borrowed = action_path.extract(&action).unwrap();
    assert!(std::ptr::eq(borrowed, original));
    assert_eq!(borrowed, "selected");
}
