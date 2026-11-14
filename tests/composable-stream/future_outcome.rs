use futures::channel::oneshot;
use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

pub const OPERATIONS: usize = 64;

#[derive(Clone, Copy, Default, Debug)]
pub enum Fault {
    #[default]
    None,
    #[cfg(test)]
    WrongId,
    #[cfg(test)]
    WrongValue,
    #[cfg(test)]
    MissingDrop,
    #[cfg(test)]
    PrematureCheckpoint,
    #[cfg(test)]
    Duplicate,
}

#[derive(Clone, Default, Debug)]
pub struct Probe(Arc<(AtomicUsize, AtomicUsize)>);

impl Probe {
    pub fn counts(&self) -> (usize, usize) {
        (
            self.0 .0.load(Ordering::Acquire),
            self.0 .1.load(Ordering::Acquire),
        )
    }
}

type Ack = oneshot::Sender<(usize, bool)>;
type DropNotice = oneshot::Sender<usize>;

pub enum Action {
    Start(Work),
    Completed {
        id: usize,
        value: usize,
        ack: Cell<Option<Ack>>,
    },
    Checkpoint(usize),
}

pub struct Work {
    id: usize,
    fault: Fault,
    probe: Probe,
    notices: Cell<Option<(Ack, DropNotice)>>,
}

pub fn prepare(id: usize, fault: Fault, probe: &Probe) -> (Action, Boundary) {
    let (ack, receive_ack) = oneshot::channel();
    let (dropped, receive_drop) = oneshot::channel();
    (
        Action::Start(Work {
            id,
            fault,
            probe: probe.clone(),
            notices: Cell::new(Some((ack, dropped))),
        }),
        Boundary {
            ack: receive_ack,
            dropped: receive_drop,
        },
    )
}

pub struct ReadyFuture {
    action: Option<Action>,
    id: usize,
    probe: Probe,
    dropped: Option<DropNotice>,
}

impl Work {
    pub fn future(&self) -> ReadyFuture {
        let (ack, dropped) = self.notices.take().expect("one future per start action");
        let (id, value) = match self.fault {
            #[cfg(test)]
            Fault::WrongId => (self.id + 1, 3 * self.id + 7),
            #[cfg(test)]
            Fault::WrongValue => (self.id, 3 * self.id + 8),
            _ => (self.id, 3 * self.id + 7),
        };
        let dropped = Some(dropped);
        #[cfg(test)]
        let dropped = if matches!(self.fault, Fault::MissingDrop) {
            None
        } else {
            dropped
        };
        ReadyFuture {
            action: Some(Action::Completed {
                id,
                value,
                ack: Cell::new(Some(ack)),
            }),
            id: self.id,
            probe: self.probe.clone(),
            dropped,
        }
    }
}

impl Future for ReadyFuture {
    type Output = Option<Action>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.probe.0 .0.fetch_add(1, Ordering::AcqRel);
        Poll::Ready(Some(self.action.take().expect("ready future polled once")))
    }
}

impl Drop for ReadyFuture {
    fn drop(&mut self) {
        self.probe.0 .1.fetch_add(1, Ordering::AcqRel);
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(self.id);
        }
    }
}

#[derive(Default, Debug)]
pub struct Trace {
    pub attempted: usize,
    pub completed: usize,
    pub results: usize,
    pub checkpoints: usize,
    pub checksum: usize,
    pub errors: usize,
}

impl Trace {
    pub fn reduce(&mut self, action: &Action) {
        match action {
            Action::Start(work) => {
                if work.id != self.attempted
                    || self.completed != self.attempted
                    || self.checkpoints != self.attempted
                {
                    self.errors += 1;
                }
                self.attempted += 1;
            }
            Action::Completed { id, value, ack } => {
                self.results += 1;
                let valid = *id == self.completed
                    && self.attempted == self.completed + 1
                    && self.checkpoints == self.completed
                    && *value == 3 * id + 7
                    && self.errors == 0;
                if valid {
                    self.completed += 1;
                    self.checksum += value;
                } else {
                    self.errors += 1;
                }
                if let Some(ack) = ack.take() {
                    let _ = ack.send((*id, valid));
                }
            }
            Action::Checkpoint(id) => {
                if *id == self.checkpoints
                    && self.attempted == id + 1
                    && self.completed == id + 1
                    && self.results == id + 1
                    && self.errors == 0
                {
                    self.checkpoints += 1;
                } else {
                    self.errors += 1;
                }
            }
        }
    }

    pub fn verify(&self, probe: &Probe) -> Result<(), &'static str> {
        self.verify_counts(probe.counts())
    }

    pub fn verify_counts(&self, counts: (usize, usize)) -> Result<(), &'static str> {
        if (
            self.attempted,
            self.completed,
            self.results,
            self.checkpoints,
            self.checksum,
            self.errors,
        ) != (OPERATIONS, OPERATIONS, OPERATIONS, OPERATIONS, 6496, 0)
            || counts != (OPERATIONS, OPERATIONS)
        {
            return Err("invalid common-outcome final ledger");
        }
        Ok(())
    }
}

pub struct Boundary {
    ack: oneshot::Receiver<(usize, bool)>,
    dropped: oneshot::Receiver<usize>,
}

impl Boundary {
    pub async fn complete(self, id: usize) -> Result<(), &'static str> {
        let ack = self
            .ack
            .await
            .map_err(|_| "result acknowledgement missing")?;
        let dropped = self
            .dropped
            .await
            .map_err(|_| "input future drop missing")?;
        if ack != (id, true) || dropped != id {
            return Err("invalid common-outcome boundary");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{executor::block_on, FutureExt};

    #[test]
    fn complete_workload_requires_exact_results_drops_and_checkpoints() {
        let probe = Probe::default();
        let mut trace = Trace::default();
        for id in 0..OPERATIONS {
            let (start, boundary) = prepare(id, Fault::None, &probe);
            trace.reduce(&start);
            let Action::Start(work) = start else {
                unreachable!()
            };
            let result = block_on(work.future()).unwrap();
            trace.reduce(&result);
            block_on(boundary.complete(id)).unwrap();
            trace.reduce(&Action::Checkpoint(id));
        }
        assert_eq!(
            (
                trace.attempted,
                trace.completed,
                trace.results,
                trace.checkpoints,
                trace.checksum,
                trace.errors
            ),
            (64, 64, 64, 64, 6496, 0)
        );
        trace.verify(&probe).unwrap();
    }

    #[test]
    fn checkpoint_before_result_is_rejected() {
        let mut trace = Trace::default();
        trace.reduce(&Action::Checkpoint(0));
        assert_eq!(trace.checkpoints, 0);
        assert_eq!(trace.errors, 1);
    }

    #[test]
    fn acknowledged_result_still_waits_for_input_future_drop() {
        let (ack, receive_ack) = oneshot::channel();
        let (dropped, receive_drop) = oneshot::channel();
        ack.send((0, true)).unwrap();
        let mut completion = Box::pin(
            Boundary {
                ack: receive_ack,
                dropped: receive_drop,
            }
            .complete(0),
        );
        assert!(completion.as_mut().now_or_never().is_none());
        dropped.send(0).unwrap();
        assert_eq!(block_on(completion), Ok(()));
    }

    #[test]
    fn missing_drop_notice_is_rejected() {
        let (ack, receive_ack) = oneshot::channel();
        let (dropped, receive_drop) = oneshot::channel();
        ack.send((0, true)).unwrap();
        drop(dropped);
        assert!(block_on(
            Boundary {
                ack: receive_ack,
                dropped: receive_drop,
            }
            .complete(0)
        )
        .is_err());
    }

    #[test]
    fn dropped_input_without_result_acknowledgement_is_rejected() {
        let (ack, receive_ack) = oneshot::channel();
        let (dropped, receive_drop) = oneshot::channel();
        drop(ack);
        dropped.send(0).unwrap();
        assert!(block_on(
            Boundary {
                ack: receive_ack,
                dropped: receive_drop,
            }
            .complete(0)
        )
        .is_err());
    }

    #[test]
    fn incorrect_acknowledgements_and_drop_identities_are_rejected() {
        for (acknowledgement, dropped_id) in [((1, true), 0), ((0, false), 0), ((0, true), 1)] {
            let (ack, receive_ack) = oneshot::channel();
            let (dropped, receive_drop) = oneshot::channel();
            ack.send(acknowledgement).unwrap();
            dropped.send(dropped_id).unwrap();
            assert!(block_on(
                Boundary {
                    ack: receive_ack,
                    dropped: receive_drop,
                }
                .complete(0)
            )
            .is_err());
        }
    }
}
