//! Input admission is not contributor membership.
use rca_core::{
    Feature, FeatureContext, Fold, FoldKey, InputGate, MountIdentity, OptionalMount, Runtime, Store,
};
use std::ops::ControlFlow;
#[path = "fold/support.rs"]
mod support;
use support::Driver;

/// Adds domain capacities independently of whether input is currently admitted.
struct Sum;
impl FoldKey for Sum {
    type Value = usize;
    fn identity() -> usize {
        0
    }
    fn reduce(accumulator: usize, next: usize) -> ControlFlow<usize, usize> {
        ControlFlow::Continue(accumulator + next)
    }
}
/// Keeps two optional contributors installed while their capacities change.
struct State {
    left: Option<usize>,
    right: Option<usize>,
}
/// Changes input admission, mounted membership or contributed values separately.
enum Action {
    Gate(InputGate, bool),
    Remove,
    Reset,
    RightZero,
    BothZero,
}
/// Applies gate and domain edits without conflating their effects on the fold.
struct Root;
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Gate(gate, open) => context.set_input_open(gate, *open).unwrap(),
            Action::Remove => state.left = None,
            Action::Reset => {
                state.left = Some(3);
                state.right = Some(2);
            }
            Action::RightZero => state.right = Some(0),
            Action::BothZero => {
                state.left = Some(0);
                state.right = Some(0);
            }
        }
    }
}
/// Supplies a child whose input gate is independent of its fold contribution.
struct Child;
impl Feature for Child {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
}
#[test]
fn closed_input_gate_preserves_three_plus_two_until_domain_removal() {
    let driver = Driver::default();
    let fold = Fold::<State, Sum>::new();
    let left_fold = fold.clone();
    let right_fold = fold.clone();
    let left = OptionalMount::new(
        |state: &State| state.left.as_ref(),
        |state: &mut State| state.left.as_mut(),
        |state: &State| {
            state.left.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let right = OptionalMount::new(
        |state: &State| state.right.as_ref(),
        |state: &mut State| state.right.as_mut(),
        |state: &State| {
            state.right.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, left_site) = Store::builder(
        State {
            left: Some(3),
            right: Some(2),
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(left, move |_, children| {
        children.contribute(&left_fold, |value| *value);
    });
    let (builder, _) = builder.mount_with_children(right, move |_, children| {
        children.contribute(&right_fold, |value| *value);
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 5);
    let gate = driver
        .finish(store.input_gate(&left_site))
        .unwrap()
        .unwrap();
    assert_eq!(format!("{gate:?}"), "InputGate { .. }");
    drop(
        driver
            .finish(store.send(Action::Gate(gate.clone(), false)))
            .unwrap(),
    );
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 5);
    drop(driver.finish(store.send(Action::Gate(gate, true))).unwrap());
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 5);
    drop(driver.finish(store.send(Action::Remove)).unwrap());
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 2);
    // Both declarations remain installed while
    // independent domain capacities change from 3+2 to 3+0 to 0+0.
    drop(driver.finish(store.send(Action::Reset)).unwrap());
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 5);
    drop(driver.finish(store.send(Action::RightZero)).unwrap());
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 3);
    drop(driver.finish(store.send(Action::BothZero)).unwrap());
    assert_eq!(driver.finish(store.fold(&fold)).unwrap(), 0);
    drop(store);
    driver.turn();
}
