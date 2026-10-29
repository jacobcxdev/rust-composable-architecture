//! Paced application-managed publication for engines without native observation.

#[path = "common.rs"]
pub(crate) mod common;

use common::{Mode, State, EXPECTED, UPDATES, WAIT};
use composable::{Effects, Reducer, Store};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};

/// Makes subscription ownership explicit in the legacy reducer's action stream.
enum Action {
    Register(mpsc::Sender<u8>),
    Set(u8),
    Unregister,
}

/// Publishes state through application glue, not a native observation API.
struct Feature {
    state: State,
    publisher: Option<mpsc::Sender<u8>>,
    publications: Arc<AtomicUsize>,
    mode: Mode,
}

impl Feature {
    fn publish(&self) {
        if let Some(sender) = &self.publisher {
            sender.send(self.state.value).unwrap();
            self.publications.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl Reducer for Feature {
    type Action = Action;
    type Output = State;

    fn reduce(&mut self, action: Action, _: impl Effects<Action>) {
        match action {
            Action::Register(sender) => {
                self.publisher = Some(sender);
                self.publish();
            }
            Action::Set(value) => {
                let repeated = self.state.value == value;
                if self.mode == Mode::PreviousValue {
                    self.publish();
                }
                self.state.update(value);
                if self.mode != Mode::PreviousValue
                    && !(self.mode == Mode::OmitRepeated && repeated)
                {
                    self.publish();
                }
            }
            Action::Unregister => {
                if self.mode != Mode::KeepRegistered {
                    self.publisher = None;
                }
            }
        }
    }
}

impl From<Feature> for State {
    fn from(feature: Feature) -> Self {
        feature.state
    }
}

#[cfg(test)]
fn exercise(mode: Mode) -> Result<(), &'static str> {
    exercise_observed(mode, &mut |_| {})
}

/// Reports boundaries after committed actions and consumes each publication before the next.
pub(crate) fn exercise_observed(
    mode: Mode,
    observe: &mut dyn FnMut(common::Point),
) -> Result<(), &'static str> {
    let publications = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::channel();
    let store = Store::with_initial(Feature {
        state: State::default(),
        publisher: None,
        publications: Arc::clone(&publications),
        mode,
    });
    observe(common::Point::Construction);

    let result = (|| {
        store.sync(Action::Register(sender));
        let mut trace = Vec::new();
        let initial = receiver
            .recv_timeout(WAIT)
            .map_err(|_| "missing publication")?;
        common::check_value(initial, 0)?;
        trace.push(initial);
        observe(common::Point::Registration);

        for (index, value) in UPDATES.into_iter().enumerate() {
            store.sync(Action::Set(value));
            let published = receiver
                .recv_timeout(WAIT)
                .map_err(|_| "missing publication")?;
            common::check_value(published, value)?;
            trace.push(published);
            if index == 1 {
                observe(common::Point::Initial);
            }
        }

        if trace != EXPECTED || publications.load(Ordering::SeqCst) != 5 {
            return Err("wrong publication count");
        }
        observe(common::Point::Checked);
        // sync acknowledges the unregister action, then the final state update.
        store.sync(Action::Unregister);
        store.sync(Action::Set(9));
        if publications.load(Ordering::SeqCst) != 5 {
            return Err("publication remained active");
        }
        if !matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
            return Err("publisher did not disconnect");
        }
        observe(common::Point::Unregistration);
        Ok(())
    })();

    // Always join the native host, including expected mutation failures.
    let state = store.into_inner();
    result?;
    if state
        != (State {
            value: 9,
            previous: 4,
            updates: 5,
        })
    {
        return Err("wrong extracted state");
    }
    Ok(())
}

#[cfg(test)]
include!("tests.rs");
