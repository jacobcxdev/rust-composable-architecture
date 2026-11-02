use crate::future_outcome::{self, Action, Fault, Probe, Trace, OPERATIONS};
use composable::{Effects, Reducer, Store};

struct ReadyReducer(Trace);

impl Reducer for ReadyReducer {
    type Action = Action;
    type Output = Trace;

    fn reduce(&mut self, action: Action, effects: impl Effects<Action>) {
        self.0.reduce(&action);
        if let Action::Start(work) = action {
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
    let probe = Probe::default();
    let store = Store::with_initial(ReadyReducer(Trace::default()));
    let completion = (|| {
        for id in 0..OPERATIONS {
            #[cfg(test)]
            if matches!(fault, Fault::PrematureCheckpoint) && id == 0 {
                store.sync(Action::Checkpoint(id));
            }
            let (action, boundary) = future_outcome::prepare(id, fault, &probe);
            store.send(action);
            futures::executor::block_on(boundary.complete(id))?;
            // The queued barrier follows checkpoint reduction. It does not join
            // the detached future task or prove its wrapper has been reclaimed.
            store.sync(Action::Checkpoint(id));
            #[cfg(test)]
            if matches!(fault, Fault::Duplicate) && id == 0 {
                store.sync(Action::Completed {
                    id,
                    value: 7,
                    ack: std::cell::Cell::new(None),
                });
            }
        }
        Ok::<_, &'static str>(())
    })();
    let state: Trace = store.into_inner();
    completion?;
    state.verify(&probe)?;
    Ok((state, probe))
}
