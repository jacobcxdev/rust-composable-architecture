//! Separates effect-send admission, waiting-storage release and terminal actor-commit costs.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this allocation test manually polls actor and effect tasks"
)]
mod support;

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, EffectSender, ReduceContext, Reducer, Store};

#[global_allocator]
/// Counts allocations in each isolated send-phase assay.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// A move-only, non-`Sync` action encodes committed values without cloning.
struct Action(Cell<u64>);

/// Exports one pending effect's sender under the requested cancellation depth.
struct CaptureSender(Arc<Mutex<Option<EffectSender<Action>>>>, usize);

impl Reducer for CaptureSender {
    type State = u64;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut u64,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if action.0.get() == 0 {
            let captured = Arc::clone(&self.0);
            let mut effect = Effect::run(move |sender| async move {
                *captured.lock().unwrap() = Some(sender);
                std::future::pending::<()>().await;
            });
            for identity in 0..self.1 {
                effect = effect.cancellable(identity);
            }
            effect
        } else {
            *state = *state * 10 + action.0.get();
            Effect::none()
        }
    }
}

/// Checks reply and generation-storage costs for effect sends.
fn effect_send_allocates_only_reply_and_nonempty_generation_storage() {
    if allocation_test::run_in_child(
        "effect_send_allocates_only_reply_and_nonempty_generation_storage",
    ) {
        return;
    }

    let commits = [0, 1, 4, 16].map(|generations| (generations, measure_effect_send(generations)));
    for (generations, (blocks, bytes)) in commits {
        assert_eq!(
            blocks, 0,
            "terminal None actor commit allocated {blocks} blocks / {bytes} bytes at generation depth {generations}"
        );
        assert_eq!(bytes, 0, "terminal None actor commit allocated bytes");
    }
}

/// Measures three send phases, then cancels work and checks committed order.
#[expect(
    clippy::too_many_lines,
    reason = "one profiling session measures admission and release before verifying final state"
)]
fn measure_effect_send(generations: usize) -> (u64, u64) {
    let captured = Arc::new(Mutex::new(None));
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(
        0,
        CaptureSender(Arc::clone(&captured), generations),
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let receipt = store.try_send(Action(Cell::new(0))).expect("start fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = futures_executor::block_on(receipt.finish()).expect("start committed");
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    let sender = captured.lock().unwrap().take().expect("captured sender");
    assert!(!task.is_finished());

    let mut admission_blocks = [0; 2];
    let mut admission_bytes = [0; 2];
    let mut retained_blocks = [0; 2];
    let mut retained_bytes = [0; 2];
    let mut send_future_bytes = [0; 2];
    let mut retry_allocations = 0;
    let mut retry_released_blocks = 0;
    let mut retry_released_bytes = 0;
    let mut commit_blocks = 0;
    let mut commit_bytes = 0;
    let mut commit_live_blocks = 0;
    let mut commit_live_bytes = 0;
    let mut profiler = None;
    for value in [1, 2, 4] {
        let prefilled = (value == 4).then(|| {
            store
                .try_send(Action(Cell::new(3)))
                .expect("prefill bounded mailbox")
        });
        let send = sender.send(Action(Cell::new(value)));
        let frame_bytes = std::mem::size_of_val(&send);
        let mut send = std::pin::pin!(send);
        // Warm the first send. One session then captures fast and full-mailbox
        // first polls separately, excluding prefill and actor processing.
        if value == 2 {
            profiler = Some(dhat::Profiler::builder().testing().build());
        }
        let before = profiler.as_ref().map(|_| dhat::HeapStats::get());
        assert!(send.as_mut().poll(&mut context).is_pending());
        if let Some(before) = before {
            let index = usize::from(value == 4);
            let admitted = dhat::HeapStats::get();
            admission_blocks[index] = admitted.total_blocks - before.total_blocks;
            admission_bytes[index] = admitted.total_bytes - before.total_bytes;
            retained_blocks[index] = admitted.curr_blocks as i128 - before.curr_blocks as i128;
            retained_bytes[index] = admitted.curr_bytes as i128 - before.curr_bytes as i128;
            send_future_bytes[index] = frame_bytes;
        }
        assert!(send.as_mut().poll(&mut context).is_pending());
        if let Some(prefilled) = prefilled {
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let task = futures_executor::block_on(prefilled.finish()).expect("prefill committed");
            assert!(task.is_finished());
            // Capacity is available, but the actor cannot commit value 4 until
            // its next poll. Admission should release both its wait listener and
            // deferred box while the public send still awaits acknowledgement.
            let before_retry = dhat::HeapStats::get();
            assert!(send.as_mut().poll(&mut context).is_pending());
            let after_retry = dhat::HeapStats::get();
            retry_allocations = after_retry.total_blocks - before_retry.total_blocks;
            retry_released_blocks =
                before_retry.curr_blocks as i128 - after_retry.curr_blocks as i128;
            retry_released_bytes = before_retry.curr_bytes as i128 - after_retry.curr_bytes as i128;
        }
        assert!(!task.is_finished());
        assert_eq!(runtime.spawn_count(), 2);
        // The response is already admitted. Measure only its actor commit,
        // excluding admission, prefill, reply polling and diagnostic output.
        let before_commit = (value == 2).then(dhat::HeapStats::get);
        let actor_poll = actor.as_mut().poll(&mut context);
        if let Some(before) = before_commit {
            let after = dhat::HeapStats::get();
            commit_blocks = after.total_blocks - before.total_blocks;
            commit_bytes = after.total_bytes - before.total_bytes;
            commit_live_blocks = after.curr_blocks as i128 - before.curr_blocks as i128;
            commit_live_bytes = after.curr_bytes as i128 - before.curr_bytes as i128;
        }
        assert!(actor_poll.is_pending());
        assert!(!task.is_finished());
        assert_eq!(runtime.spawn_count(), 2);
        assert!(matches!(
            send.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
    }
    drop(profiler);

    task.cancel();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(1234))
    ));

    // Record heap and public future storage separately; exact byte sizes are
    // diagnostic, not portable bounds or claims about total retained memory.
    for (index, name) in ["fast", "full"].into_iter().enumerate() {
        eprintln!(
            "effect send {name}, generations={generations}: blocks={} bytes={} retained_blocks_delta={} retained_bytes_delta={} public_send_future_bytes={}",
            admission_blocks[index],
            admission_bytes[index],
            retained_blocks[index],
            retained_bytes[index],
            send_future_bytes[index],
        );
    }
    let budget = 1 + u64::from(generations != 0);
    assert!(
        admission_blocks[0] <= budget,
        "effect send fast admission allocated {} blocks; budget is {budget}",
        admission_blocks[0],
    );
    eprintln!(
        "effect send admitted fallback: new_blocks={retry_allocations} released_blocks={retry_released_blocks} released_bytes={retry_released_bytes}"
    );
    assert_eq!(retry_allocations, 0, "admitted fallback allocated again");
    assert!(
        retry_released_blocks >= 2,
        "admitted fallback released {retry_released_blocks} blocks / {retry_released_bytes} bytes; listener and deferred box must both be released before acknowledgement"
    );
    eprintln!(
        "terminal None actor commit, generations={generations}: blocks={commit_blocks} bytes={commit_bytes} live_blocks_delta={commit_live_blocks} live_bytes_delta={commit_live_bytes}"
    );
    (commit_blocks, commit_bytes)
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "effect_send_allocates_only_reply_and_nonempty_generation_storage",
    effect_send_allocates_only_reply_and_nonempty_generation_storage,
)];

include!("support/allocation_main.rs");
