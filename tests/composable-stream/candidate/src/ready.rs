use crate::ready_future::{self, Action, Fault, Point, Probe, Trace, OPERATIONS};
use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;

struct ReadyReducer;

impl Reducer for ReadyReducer {
    type State = Trace;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Trace,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.reduce(action);
        if let Action::Start(work) = action {
            let future = work.future();
            return Effect::run(move |sender| async move {
                if let Some(action) = future.await {
                    // A failed delivery is rejected by the missing reducer acknowledgement.
                    let _ = sender.send(action).await;
                }
            });
        }
        Effect::none()
    }
}

pub fn run(fault: Fault) -> Result<(Trace, Probe), &'static str> {
    run_observed(fault, |_| {})
}

pub fn run_observed(
    fault: Fault,
    mut observe: impl FnMut(Point),
) -> Result<(Trace, Probe), &'static str> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "runtime build failed")?;
    let probe = Probe::default();
    let result = runtime.block_on(async {
        let store = Store::builder(
            Trace::default(),
            ReadyReducer,
            TokioRuntime::current().map_err(|_| "runtime adapter failed")?,
        )
        .build()
        .map_err(|_| "store build failed")?;
        observe(Point::Construction);
        let completion = async {
            for id in 0..OPERATIONS {
                let (action, boundary) = ready_future::prepare(id, fault, &probe);
                let task = store
                    .send(action)
                    .await
                    .map_err(|_| "start delivery failed")?;
                tokio::time::timeout(crate::common::TIMEOUT, task.finish())
                    .await
                    .map_err(|_| "causal finish timed out")?;
                boundary.complete(id, &probe, false)?;
                #[cfg(test)]
                if matches!(fault, Fault::Duplicate) && id == 0 {
                    store
                        .send(Action::Completed {
                            id,
                            value: 7,
                            ack: None,
                        })
                        .await
                        .map_err(|_| "duplicate delivery failed")?
                        .finish()
                        .await;
                }
                // The boundary and caller-owned task/receipt temporaries are gone.
                if id == 19 {
                    observe(Point::Initial);
                }
                if id == OPERATIONS - 1 {
                    observe(Point::Checked);
                }
            }
            Ok::<_, &'static str>(())
        }
        .await;
        // Extract and stop the actor even when an iteration was rejected.
        let state = store
            .shutdown_into_state()
            .await
            .map_err(|_| "state extraction failed")?;
        completion?;
        Ok::<_, &'static str>(state)
    });
    drop(runtime);
    let state = result?;
    state.verify(&probe)?;
    Ok((state, probe))
}

#[cfg(test)]
#[path = "../../ready_native_tests.rs"]
mod tests;
