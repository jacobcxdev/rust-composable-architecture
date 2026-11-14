use crate::ready_future::{self, Action, Fault, Point, Probe, Trace, OPERATIONS};
use composable::{Effects, Reducer, Store};

struct ReadyReducer(Trace);

impl Reducer for ReadyReducer {
    type Action = Action;
    type Output = Trace;

    fn reduce(&mut self, action: Action, effects: impl Effects<Action>) {
        self.0.reduce(&action);
        if let Action::Start(work) = action {
            // Native future detaches; no legacy Task::join completion claim.
            effects.future(work.future());
        }
    }
}

impl From<ReadyReducer> for Trace {
    fn from(reducer: ReadyReducer) -> Self {
        reducer.0
    }
}

pub fn run(fault: Fault) -> Result<(Trace, Probe), &'static str> {
    run_observed(fault, |_| {})
}

pub fn run_observed(
    fault: Fault,
    mut observe: impl FnMut(Point),
) -> Result<(Trace, Probe), &'static str> {
    let probe = Probe::default();
    let store = Store::with_initial(ReadyReducer(Trace::default()));
    observe(Point::Construction);
    let completion = (|| {
        for id in 0..OPERATIONS {
            let (action, boundary) = ready_future::prepare(id, fault, &probe);
            store.send(action);
            boundary.complete(id, &probe, true)?;
            #[cfg(test)]
            if matches!(fault, Fault::Duplicate) && id == 0 {
                store.send(Action::Completed {
                    id,
                    value: 7,
                    ack: None,
                });
            }
            // Caller coordination is released; reducer acknowledgement is not
            // a claim that the detached host is globally quiescent.
            if id == 19 {
                observe(Point::Initial);
            }
            if id == OPERATIONS - 1 {
                observe(Point::Checked);
            }
        }
        Ok::<_, &'static str>(())
    })();
    // Result acknowledgement and input Drop precede extraction; into_inner joins
    // the native host, including on a rejected iteration.
    let state: Trace = store.into_inner();
    completion?;
    state.verify(&probe)?;
    Ok((state, probe))
}

#[cfg(test)]
#[path = "ready_native_tests.rs"]
mod tests;
