//! Nested mapping admission costs, deferred commit and cancellation forwarding.
//!
//! Move-only actions cross two mappings. Final state proves their order and values.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this allocation test manually polls actor and effect tasks"
)]
mod support;

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, EffectSender, ReduceContext, Reducer, Store};

#[global_allocator]
/// Measures only the warmed send's first poll in an isolated child.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Supplies a move-only, non-thread-safe value to the inner mapping.
struct ChildAction(Cell<u64>);
/// Carries the inner mapping's result into the outer mapping.
struct MiddleAction(Cell<u64>);

/// Starts sender capture or commits the fully mapped value.
enum RootAction {
    Start,
    Value(Cell<u64>),
}

/// Exports the actual nested sender while its originating effect remains pending.
struct CaptureSender {
    captured: Arc<Mutex<Option<EffectSender<ChildAction>>>>,
    mappings: Arc<AtomicUsize>,
}

impl Reducer for CaptureSender {
    type State = u64;
    type Action = RootAction;

    fn reduce(
        &mut self,
        state: &mut u64,
        action: &RootAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<RootAction> {
        match action {
            RootAction::Start => {
                let captured = Arc::clone(&self.captured);
                let inner_mappings = Arc::clone(&self.mappings);
                let outer_mappings = Arc::clone(&self.mappings);
                Effect::run(move |sender| async move {
                    *captured.lock().unwrap() = Some(sender);
                    std::future::pending::<()>().await;
                })
                .map(move |child: ChildAction| {
                    inner_mappings.fetch_add(1, Ordering::AcqRel);
                    MiddleAction(Cell::new(child.0.into_inner() + 10))
                })
                .map(move |middle: MiddleAction| {
                    outer_mappings.fetch_add(1, Ordering::AcqRel);
                    RootAction::Value(Cell::new(middle.0.into_inner() * 2))
                })
            }
            RootAction::Value(value) => {
                *state = *state * 100 + value.get();
                Effect::none()
            }
        }
    }
}

#[test]
fn nested_mapped_sends_do_not_allocate_forwarding_futures() {
    if allocation_test::run_in_child("nested_mapped_sends_do_not_allocate_forwarding_futures") {
        return;
    }

    let captured = Arc::new(Mutex::new(None));
    let mappings = Arc::new(AtomicUsize::new(0));
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(
        0,
        CaptureSender {
            captured: Arc::clone(&captured),
            mappings: Arc::clone(&mappings),
        },
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let receipt = store.try_send(RootAction::Start).expect("start fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = futures_executor::block_on(receipt.finish()).expect("start committed");
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    let sender = captured
        .lock()
        .unwrap()
        .take()
        .expect("captured child sender");
    assert!(!task.is_finished());

    let mut admission_blocks = 0;
    let mut admission_bytes = 0;
    for (index, value) in [1, 2].into_iter().enumerate() {
        let send = sender.send(ChildAction(Cell::new(value)));
        let mut send = std::pin::pin!(send);
        assert_eq!(mappings.load(Ordering::Acquire), index * 2);
        // Warm the first real send. The only profiler session measures the
        // second send's first poll, with no actor work in that window.
        let profiler = (index == 1).then(|| dhat::Profiler::builder().testing().build());
        let before = profiler.as_ref().map(|_| dhat::HeapStats::get());
        assert!(send.as_mut().poll(&mut context).is_pending());
        if let Some(before) = before {
            let admitted = dhat::HeapStats::get();
            admission_blocks = admitted.total_blocks - before.total_blocks;
            admission_bytes = admitted.total_bytes - before.total_bytes;
        }
        drop(profiler);
        assert_eq!(mappings.load(Ordering::Acquire), (index + 1) * 2);
        assert!(send.as_mut().poll(&mut context).is_pending());
        assert_eq!(mappings.load(Ordering::Acquire), (index + 1) * 2);
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert!(matches!(
            send.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
    }

    // A mapped send still waits for actor validation and forwards cancellation.
    let cancelled = {
        let send = sender.send(ChildAction(Cell::new(3)));
        let mut send = std::pin::pin!(send);
        assert_eq!(mappings.load(Ordering::Acquire), 4);
        assert!(send.as_mut().poll(&mut context).is_pending());
        task.cancel();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Err(error)) = send.as_mut().poll(&mut context) else {
            panic!("cancelled lineage must reject the queued mapped action");
        };
        error
    };
    assert!(cancelled.is_cancelled());
    assert_eq!(mappings.load(Ordering::Acquire), 6);
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(2224))
    ));

    eprintln!("nested mapped send admission: blocks={admission_blocks} bytes={admission_bytes}");
    assert!(
        admission_blocks <= 2,
        "nested mapped send allocated {admission_blocks} blocks; budget is 2",
    );
}
