//! Paced native observation, including explicit drop and state-extraction boundaries.

#[path = "common.rs"]
pub(crate) mod common;

use common::{Mode, State, EXPECTED, UPDATES, WAIT};
use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Feature;

impl Reducer for Feature {
    type State = State;
    type Action = u8;

    fn reduce(&mut self, state: &mut State, action: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
        state.update(*action);
        Effect::none()
    }
}

#[cfg(test)]
fn exercise(mode: Mode) -> Result<(), &'static str> {
    exercise_observed(mode, &mut |_| {})
}

/// Reports lifecycle boundaries only after the corresponding native work completes.
pub(crate) fn exercise_observed(
    mode: Mode,
    observe: &mut dyn FnMut(common::Point),
) -> Result<(), &'static str> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let selector_calls = Arc::clone(&calls);
        let store = Store::new(State::default(), Feature, TokioRuntime::current().unwrap());
        observe(common::Point::Construction);

        let selector = move |state: &State| {
            selector_calls.fetch_add(1, Ordering::SeqCst);
            if mode == Mode::PreviousValue {
                state.previous
            } else {
                state.value
            }
        };

        let scenario = async {
            let values = if mode == Mode::OmitRepeated {
                store.observe_distinct(selector).await
            } else {
                store.observe(selector).await
            }
            .map_err(|_| "registration failed")?;
            let mut values = Some(values);
            let mut trace = Vec::new();
            let initial = tokio::time::timeout(WAIT, values.as_mut().unwrap().next())
                .await
                .map_err(|_| "missing publication")?
                .ok_or("observation closed")?;
            common::check_value(initial, 0)?;
            trace.push(initial);
            observe(common::Point::Registration);

            for (index, value) in UPDATES.into_iter().enumerate() {
                let _ = store.send(value).await.map_err(|_| "send failed")?;
                let published = tokio::time::timeout(WAIT, values.as_mut().unwrap().next())
                    .await
                    .map_err(|_| "missing publication")?
                    .ok_or("observation closed")?;
                common::check_value(published, value)?;
                trace.push(published);
                if index == 1 {
                    observe(common::Point::Initial);
                }
            }

            if trace != EXPECTED || calls.load(Ordering::SeqCst) != 5 {
                return Err("wrong publication count");
            }
            observe(common::Point::Checked);
            if mode != Mode::KeepRegistered {
                drop(values.take());
            }
            // Ordered read is a barrier, not a new commit/selector invocation.
            store.read(|_| ()).await.map_err(|_| "barrier failed")?;
            let _ = store.send(9).await.map_err(|_| "send failed")?;
            store.read(|_| ()).await.map_err(|_| "barrier failed")?;
            if calls.load(Ordering::SeqCst) != 5 {
                return Err("publication remained active");
            }
            observe(common::Point::Unregistration);
            Ok(())
        };

        let result = tokio::time::timeout(WAIT * 3, scenario)
            .await
            .map_err(|_| "scenario timed out");
        // Close/extract even when a deliberate mutation fails.
        let state = tokio::time::timeout(WAIT, store.shutdown_into_state())
            .await
            .map_err(|_| "shutdown timed out")?
            .map_err(|_| "extraction failed")?;
        result??;
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
    });
    drop(runtime);
    result
}

#[cfg(test)]
include!("tests.rs");
