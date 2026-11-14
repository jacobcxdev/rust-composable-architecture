use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    task::{Context, Poll},
};

pub const OPERATIONS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    Construction,
    Initial,
    Checked,
}

#[derive(Clone, Copy, Default)]
pub enum Fault {
    #[default]
    None,
    #[cfg(test)]
    WrongId,
    #[cfg(test)]
    WrongValue,
    #[cfg(test)]
    Duplicate,
}

#[derive(Clone, Default, Debug)]
pub struct Probe {
    polls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl Probe {
    pub fn counts(&self) -> (usize, usize) {
        (
            self.polls.load(Ordering::Acquire),
            self.drops.load(Ordering::Acquire),
        )
    }
}

pub enum Action {
    Start(Work),
    Completed {
        id: usize,
        value: usize,
        ack: Option<mpsc::Sender<(usize, bool)>>,
    },
}

pub struct Work {
    id: usize,
    fault: Fault,
    probe: Probe,
    ack: mpsc::Sender<(usize, bool)>,
    dropped: mpsc::Sender<usize>,
}

pub struct Boundary {
    ack: mpsc::Receiver<(usize, bool)>,
    dropped: mpsc::Receiver<usize>,
}

pub fn prepare(id: usize, fault: Fault, probe: &Probe) -> (Action, Boundary) {
    let (ack, receive_ack) = mpsc::channel();
    let (dropped, receive_drop) = mpsc::channel();
    (
        Action::Start(Work {
            id,
            fault,
            probe: probe.clone(),
            ack,
            dropped,
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
    dropped: mpsc::Sender<usize>,
}

impl Work {
    pub fn future(&self) -> ReadyFuture {
        let (id, value) = match self.fault {
            #[cfg(test)]
            Fault::WrongId => (self.id + 1, 3 * self.id + 7),
            #[cfg(test)]
            Fault::WrongValue => (self.id, 3 * self.id + 8),
            _ => (self.id, 3 * self.id + 7),
        };
        ReadyFuture {
            action: Some(Action::Completed {
                id,
                value,
                ack: Some(self.ack.clone()),
            }),
            id: self.id,
            probe: self.probe.clone(),
            dropped: self.dropped.clone(),
        }
    }
}

impl Future for ReadyFuture {
    type Output = Option<Action>;
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Action>> {
        self.probe.polls.fetch_add(1, Ordering::AcqRel);
        Poll::Ready(Some(self.action.take().expect("ready future polled once")))
    }
}

impl Drop for ReadyFuture {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::AcqRel);
        let _ = self.dropped.send(self.id);
    }
}

#[derive(Default, Debug)]
pub struct Trace {
    pub attempted: usize,
    pub completed: usize,
    pub results: usize,
    pub checksum: usize,
    pub errors: usize,
}

impl Trace {
    pub fn reduce(&mut self, action: &Action) {
        match action {
            Action::Start(work) => {
                if work.id != self.attempted || self.completed != self.attempted {
                    self.errors += 1;
                }
                self.attempted += 1;
            }
            Action::Completed { id, value, ack } => {
                self.results += 1;
                let valid = *id == self.completed
                    && self.attempted == self.completed + 1
                    && *value == 3 * id + 7;
                if valid {
                    self.completed += 1;
                    self.checksum += value;
                } else {
                    self.errors += 1;
                }
                if let Some(ack) = ack {
                    let _ = ack.send((*id, valid));
                }
            }
        }
    }

    pub fn verify(&self, probe: &Probe) -> Result<(), &'static str> {
        self.verify_counts(probe.counts())
    }

    pub fn verify_counts(&self, counts: (usize, usize)) -> Result<(), &'static str> {
        if self.attempted != OPERATIONS
            || self.completed != OPERATIONS
            || self.results != OPERATIONS
            || self.checksum != 6496
            || self.errors != 0
            || counts != (OPERATIONS, OPERATIONS)
        {
            return Err("invalid ready-future final ledger");
        }
        Ok(())
    }

    pub fn emit(&self, engine: &str, completion: &str, probe: &Probe) {
        println!("{}", self.json(engine, completion, probe.counts()));
    }

    pub fn json(&self, engine: &str, completion: &str, counts: (usize, usize)) -> String {
        format!("{{\"schema\":\"composable-ready-future-v1\",\"engine\":\"{engine}\",\"operations_expected\":{OPERATIONS},\"operations_attempted\":{},\"operations_completed\":{},\"results\":{},\"checksum\":{},\"future_polls\":{},\"future_drops\":{},\"errors\":{},\"state_extracted\":true,\"host_stopped\":true,\"completion\":\"{completion}\"}}", self.attempted, self.completed, self.results, self.checksum, counts.0, counts.1, self.errors)
    }
}

impl Boundary {
    pub fn complete(self, id: usize, probe: &Probe, wait: bool) -> Result<(), &'static str> {
        let ack = if wait {
            self.ack.recv_timeout(crate::common::TIMEOUT).ok()
        } else {
            self.ack.try_recv().ok()
        }
        .ok_or("result acknowledgement missing")?;
        let dropped = if wait {
            self.dropped.recv_timeout(crate::common::TIMEOUT).ok()
        } else {
            self.dropped.try_recv().ok()
        }
        .ok_or("input future drop missing")?;
        self.verify(id, probe, ack, dropped)
    }

    fn verify(
        &self,
        id: usize,
        probe: &Probe,
        ack: (usize, bool),
        dropped: usize,
    ) -> Result<(), &'static str> {
        if ack != (id, true)
            || dropped != id
            || probe.polls.load(Ordering::Acquire) != id + 1
            || probe.drops.load(Ordering::Acquire) != id + 1
            || self.ack.try_recv().is_ok()
            || self.dropped.try_recv().is_ok()
        {
            return Err("invalid ready-future iteration boundary");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_results_cannot_produce_a_success_ledger() {
        Trace::default()
            .verify(&Probe::default())
            .expect_err("missing workload rejected");
    }

    #[test]
    fn polled_but_not_dropped_future_is_not_an_iteration_boundary() {
        let probe = Probe::default();
        let (start, boundary) = prepare(0, Fault::None, &probe);
        let mut trace = Trace::default();
        trace.reduce(&start);
        let Action::Start(work) = start else {
            unreachable!()
        };
        let mut future = work.future();
        let waker = futures::task::noop_waker();
        let Poll::Ready(Some(action)) =
            Pin::new(&mut future).poll(&mut Context::from_waker(&waker))
        else {
            unreachable!()
        };
        trace.reduce(&action);
        boundary
            .complete(0, &probe, false)
            .expect_err("input future still owned");
        drop(future);
    }

    #[test]
    fn dropped_future_without_result_ack_is_not_complete() {
        let probe = Probe::default();
        let (start, boundary) = prepare(0, Fault::None, &probe);
        let Action::Start(work) = start else {
            unreachable!()
        };
        drop(work.future());
        drop(work);
        boundary
            .complete(0, &probe, false)
            .expect_err("missing poll and result rejected");
    }
}
