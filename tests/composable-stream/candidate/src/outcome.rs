use crate::future_outcome::{self, Action, Fault, Probe, Trace, OPERATIONS};
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
            return Effect::future(work.future());
        }
        Effect::none()
    }
}

pub fn run(fault: Fault) -> Result<(Trace, Probe), &'static str> {
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
        let completion = async {
            for id in 0..OPERATIONS {
                #[cfg(test)]
                if matches!(fault, Fault::PrematureCheckpoint) && id == 0 {
                    drop(
                        store
                            .send(Action::Checkpoint(id))
                            .await
                            .map_err(|_| "early checkpoint delivery failed")?,
                    );
                }
                let (action, boundary) = future_outcome::prepare(id, fault, &probe);
                // This workload does not request causal completion. Awaiting the
                // shared notices lets the current-thread runtime drive the effect.
                drop(
                    store
                        .send(action)
                        .await
                        .map_err(|_| "start delivery failed")?,
                );
                boundary.complete(id).await?;
                // Effect-free commit acknowledgement follows reducer return,
                // not necessarily detached task-wrapper reclamation.
                drop(
                    store
                        .send(Action::Checkpoint(id))
                        .await
                        .map_err(|_| "checkpoint delivery failed")?,
                );
                #[cfg(test)]
                if matches!(fault, Fault::Duplicate) && id == 0 {
                    drop(
                        store
                            .send(Action::Completed {
                                id,
                                value: 7,
                                ack: std::cell::Cell::new(None),
                            })
                            .await
                            .map_err(|_| "duplicate delivery failed")?,
                    );
                }
            }
            Ok::<_, &'static str>(())
        }
        .await;
        // Always stop and extract state, including when a boundary was rejected.
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
