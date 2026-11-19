//! Composition, scoped Store and collection workloads.
//!
//! Persistent Store rows exclude construction and shutdown but include admission,
//! causal completion and worker scheduling. Generated-input rows exclude fixture
//! construction and returned-owner destruction; their mutation and assertions are
//! timed. Collection controls compare public operations, not equal metadata or
//! capacity contracts. Allocation mode retains the returned output at its snapshot
//! and counts all threads in the process; retained bytes are not a leak verdict.

use rca_core::combine;

include!("composition/fixtures.rs");

fn main() {
    #[cfg(feature = "allocation-measurement")]
    if let Ok(scenario) = std::env::var("RCA_COMPOSITION_ALLOCATION_SCENARIO")
        .or_else(|_| std::env::var("RCA_SCOPED_ALLOCATION_SCENARIO"))
    {
        let samples = std::env::var("RCA_COMPOSITION_ALLOCATION_SAMPLES")
            .or_else(|_| std::env::var("RCA_SCOPED_ALLOCATION_SAMPLES"))
            .map_or(Ok(1), |value| value.parse::<usize>())
            .expect("allocation sample count must be a positive integer");
        assert!(samples > 0, "allocation sample count must be positive");
        run_allocation_scenario(&scenario, samples);
        return;
    }
    divan::main();
}

#[divan::bench]
fn map_none(bencher: divan::Bencher<'_, '_>) {
    bencher.bench(|| {
        divan::black_box(Effect::<u8>::none().map(u16::from));
    });
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, max_time = 0.5)]
fn effect_map_depth(bencher: divan::Bencher<'_, '_>, depth: usize) {
    effect_map_bench(bencher, depth, 1);
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 50, max_time = 0.5)]
fn effect_map_output_rate(bencher: divan::Bencher<'_, '_>, outputs: usize) {
    effect_map_bench(bencher, 4, outputs);
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, items_count = 1_u64)]
fn direct(bencher: divan::Bencher<'_, '_>, depth: usize) {
    bench_store(bencher, Leaf, depth, 1);
}

#[divan::bench(
    args = [1_usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
    sample_count = 50,
    items_count = 1_u64
)]
fn combine_arity(bencher: divan::Bencher<'_, '_>, arity: usize) {
    macro_rules! bench_arity {
        ($($leaf:expr),+ $(,)?) => {
            bench_store(bencher, combine(($($leaf,)+)), 0, arity)
        };
    }

    match arity {
        1 => bench_arity!(Leaf),
        2 => bench_arity!(Leaf, Leaf),
        3 => bench_arity!(Leaf, Leaf, Leaf),
        4 => bench_arity!(Leaf, Leaf, Leaf, Leaf),
        5 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf),
        6 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        7 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        8 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        9 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        10 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        11 => bench_arity!(
            Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf
        ),
        12 => bench_arity!(
            Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf,
        ),
        _ => unreachable!("Divan supplies only supported Combine arities"),
    }
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, items_count = 1_u64)]
fn matching_scope(bencher: divan::Bencher<'_, '_>, depth: usize) {
    match depth {
        1 => bench_store(bencher, depth_1(), depth, 1),
        4 => bench_store(bencher, depth_4(), depth, 1),
        16 => bench_store(bencher, depth_16(), depth, 1),
        _ => unreachable!("Divan supplies only supported scope depths"),
    }
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, items_count = 1_u64)]
fn nonmatching_scope(bencher: divan::Bencher<'_, '_>, depth: usize) {
    let action_depth = depth - 1;
    match depth {
        1 => bench_store(bencher, depth_1(), action_depth, 0),
        4 => bench_store(bencher, depth_4(), action_depth, 0),
        16 => bench_store(bencher, depth_16(), action_depth, 0),
        _ => unreachable!("Divan supplies only supported scope depths"),
    }
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn optional_hit(bencher: divan::Bencher<'_, '_>) {
    let fixture = optional_fixture(true);
    finish_task(&fixture.commit(LifecycleAction::Child(LifecycleChildAction::Hit)));
    let mut hits = 1;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(LifecycleAction::Child(
            LifecycleChildAction::Hit,
        ))));
        hits += 1;
    });
    assert_eq!(
        futures_executor::block_on(
            fixture
                .store
                .read(|state| { state.child.as_ref().map(|child| child.hits) })
        )
        .expect("final optional state"),
        Some(hits)
    );
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn optional_miss(bencher: divan::Bencher<'_, '_>) {
    let fixture = optional_fixture(true);
    finish_task(&fixture.commit(LifecycleAction::Miss));
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(LifecycleAction::Miss)));
    });
    assert_eq!(
        futures_executor::block_on(
            fixture
                .store
                .read(|state| { state.child.as_ref().map(|child| child.hits) })
        )
        .expect("final optional state"),
        Some(0)
    );
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn optional_enter(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(|| optional_fixture(false))
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(divan::black_box(LifecycleAction::Enter)));
            fixture
        });
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn optional_leave(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(|| optional_fixture(true))
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(divan::black_box(LifecycleAction::Leave)));
            fixture
        });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn case_hit(bencher: divan::Bencher<'_, '_>) {
    let fixture = case_fixture();
    finish_task(&fixture.commit(ScreenAction::Detail(LifecycleChildAction::Hit)));
    let mut hits = 1;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(ScreenAction::Detail(
            LifecycleChildAction::Hit,
        ))));
        hits += 1;
    });
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| match state {
            ScreenState::Detail(child) => Some(child.hits),
            ScreenState::List => None,
        }))
        .expect("final case state"),
        Some(hits)
    );
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn case_miss(bencher: divan::Bencher<'_, '_>) {
    let fixture = case_fixture();
    finish_task(&fixture.commit(ScreenAction::Miss));
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(ScreenAction::Miss)));
    });
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| match state {
            ScreenState::Detail(child) => Some(child.hits),
            ScreenState::List => None,
        }))
        .expect("final case state"),
        Some(0)
    );
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn case_switch(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(case_fixture)
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(divan::black_box(ScreenAction::Switch)));
            fixture
        });
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn descendant_cancellation(bencher: divan::Bencher<'_, '_>, task_count: usize) {
    bencher
        .with_inputs(|| CancellationFixture::new(task_count))
        .bench_local_values(|fixture| {
            fixture.cancel();
            fixture
        });
}

/// Instantiates the same operation rows for each explicitly different control.
macro_rules! collection_benches {
    ($module:ident, $collection:ty) => {
        mod $module {
            use super::*;

            #[divan::bench(args = [10_usize, 1_000, 100_000], sample_count = 50, max_time = 0.5, items_count = 1_u64)]
            fn lookup(bencher: divan::Bencher<'_, '_>, size: usize) {
                bench_collection_lookup::<$collection>(bencher, size);
            }

            macro_rules! mutation_bench {
                ($name:ident, $mutation:expr) => {
                    #[divan::bench(args = [10_usize, 1_000, 100_000], sample_count = 10, sample_size = 4, max_time = 0.5, skip_ext_time = true, items_count = 1_u64)]
                    fn $name(bencher: divan::Bencher<'_, '_>, size: usize) {
                        bench_collection_mutation::<$collection>(bencher, size, $mutation);
                    }
                };
            }

            mutation_bench!(insert, CollectionMutation::Insert);
            mutation_bench!(ordered_remove, CollectionMutation::OrderedRemove);
            mutation_bench!(swap_remove, CollectionMutation::SwapRemove);
            mutation_bench!(move_last_to_first, CollectionMutation::Move);
            mutation_bench!(clear, CollectionMutation::Clear);
        }
    };
}

collection_benches!(keyed_state_collection, KeyedStateCollection);
collection_benches!(indexmap_control, IndexMapCollection);
collection_benches!(vec_hashmap_control, VecHashCollection);
collection_benches!(vec_btreemap_control, VecBTreeCollection);

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true
)]
fn keyed_initial_mount(bencher: divan::Bencher<'_, '_>, size: usize) {
    bencher
        .with_inputs(|| keyed_fixture(size))
        .bench_local_values(|fixture| {
            warm_keyed_fixture(&fixture);
            fixture
        });
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 50,
    max_time = 0.5,
    items_count = 1_u64
)]
fn keyed_matching_route(bencher: divan::Bencher<'_, '_>, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    let mut iterations = 0;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
            divan::black_box(id),
            KeyedBenchChildAction::Hit,
        ))));
        iterations += 1;
    });
    let actual = futures_executor::block_on(fixture.store.read(move |state| {
        (
            state.rows.get(&id).copied(),
            state.outputs,
            state.rows.len(),
        )
    }))
    .expect("final keyed state");
    assert_eq!(actual, (Some(iterations), 0, size));
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 50,
    max_time = 0.5,
    items_count = 1_u64
)]
fn keyed_unrelated_reconciliation(bencher: divan::Bencher<'_, '_>, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(KeyedBenchAction::Unrelated)));
    });
    let actual = futures_executor::block_on(fixture.store.read(|state| {
        (
            state.rows.values().copied().sum::<usize>(),
            state.outputs,
            state.rows.len(),
        )
    }))
    .expect("final keyed state");
    assert_eq!(actual, (0, 0, size));
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 50,
    max_time = 0.5,
    items_count = 2_u64
)]
fn keyed_two_output_id_mapping(bencher: divan::Bencher<'_, '_>, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    let mut iterations = 0;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
            divan::black_box(id),
            KeyedBenchChildAction::EmitTwo,
        ))));
        iterations += 1;
    });
    let actual = futures_executor::block_on(
        fixture
            .store
            .read(move |state| (state.rows.get(&id).copied(), state.outputs)),
    )
    .expect("final mapped output state");
    assert_eq!(actual, (Some(0), iterations * 2));
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 10,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn keyed_one_removal_cancellation(bencher: divan::Bencher<'_, '_>, size: usize) {
    bencher
        .with_inputs(|| KeyedRemovalCancellationFixture::new(size))
        .bench_local_values(|fixture| {
            fixture.cancel();
            fixture
        });
}

#[divan::bench(
    args = [1_000_usize, 100_000],
    sample_count = 10,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_000_u64
)]
fn keyed_remove_1_000_active_entries(bencher: divan::Bencher<'_, '_>, size: usize) {
    bencher
        .with_inputs(|| {
            let fixture = keyed_fixture(size);
            warm_keyed_fixture(&fixture);
            fixture
        })
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(KeyedBenchAction::RemoveFirst(1_000)));
            fixture
        });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_required_construction(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    bencher.bench_local(|| divan::black_box(required_scoped_handle(&fixture.store)));
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_optional_construction(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    bencher.bench_local(|| {
        divan::black_box(futures_executor::block_on(optional_scoped_handle(
            &fixture.store,
        )))
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_keyed_construction(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    bencher.bench_local(|| {
        divan::black_box(futures_executor::block_on(keyed_scoped_handle(
            &fixture.store,
        )))
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_send(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut iterations = 0;
    bencher.bench_local(|| {
        let task = futures_executor::block_on(
            handle.send(divan::black_box(ScopedBenchChildAction::Increment)),
        )
        .expect("scoped action commits");
        finish_task(&task);
        iterations += 1;
    });
    assert_eq!(
        futures_executor::block_on(handle.read(|child| child.value)).expect("final scoped state"),
        iterations
    );
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 32_u64
)]
fn scoped_concurrent_senders_32(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(ConcurrentSendFixture::new)
        .bench_local_values(|fixture| {
            fixture.run();
            fixture
        });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_try_send(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut iterations = 0;
    bencher.bench_local(|| {
        let receipt = handle
            .try_send(divan::black_box(ScopedBenchChildAction::Increment))
            .expect("benchmark mailbox remains available");
        let task = futures_executor::block_on(receipt.finish()).expect("scoped action commits");
        finish_task(&task);
        iterations += 1;
    });
    assert_eq!(
        futures_executor::block_on(handle.read(|child| child.value)).expect("final scoped state"),
        iterations
    );
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_try_send_full(bencher: divan::Bencher<'_, '_>) {
    let fixture = full_mailbox_fixture();
    bencher.bench_local(|| {
        let error = fixture
            .handle
            .try_send(divan::black_box(ScopedBenchChildAction::Increment))
            .expect_err("paused one-slot mailbox stays full");
        assert!(error.is_full());
        divan::black_box(error.into_action());
    });
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn scoped_try_send_recovery(bencher: divan::Bencher<'_, '_>) {
    // Keep one pool alive across input batches, outside the measured operation.
    let _pool_owner = PausedDriver::new();

    bencher
        .with_inputs(full_mailbox_fixture)
        .bench_local_values(|mut fixture| {
            fixture.driver.start();
            let first = fixture.first.take().expect("one paused receipt");
            let first_task = futures_executor::block_on(first.finish())
                .expect("paused action commits after recovery");
            finish_task(&first_task);
            let receipt = fixture
                .handle
                .try_send(ScopedBenchChildAction::Increment)
                .expect("mailbox accepts after recovery");
            let task =
                futures_executor::block_on(receipt.finish()).expect("recovery action commits");
            finish_task(&task);
            fixture
        });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_read(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    bencher.bench_local(|| {
        divan::black_box(
            futures_executor::block_on(handle.read(|child| child.value))
                .expect("scoped read succeeds"),
        );
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_observe_always(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe(|child| child.value))
        .expect("observation registers");
    consume_initial(&mut observation);
    bencher.bench_local(|| {
        let task = fixture.commit(ScopedBenchAction::ParentOnly);
        finish_task(&task);
        let value = futures_executor::block_on(observation.next());
        assert_eq!(value, Some(0));
        divan::black_box(value);
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_observe_distinct_equal(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe_distinct(|child| child.value))
        .expect("distinct observation registers");
    consume_initial(&mut observation);
    bencher.bench_local(|| finish_task(&fixture.commit(ScopedBenchAction::ParentOnly)));
    let mut next = std::pin::pin!(observation.next());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(next.as_mut().poll(&mut context).is_pending());
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_observe_distinct_non_equal(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe_distinct(|child| child.value))
        .expect("distinct observation registers");
    consume_initial(&mut observation);
    let mut expected = 0;
    bencher.bench_local(|| {
        let task = futures_executor::block_on(handle.send(ScopedBenchChildAction::Increment))
            .expect("scoped action commits");
        finish_task(&task);
        expected += 1;
        let value = futures_executor::block_on(observation.next());
        assert_eq!(value, Some(expected));
        divan::black_box(value);
    });
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn scoped_invalidation(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(invalidation_fixture)
        .bench_local_values(|mut fixture| {
            finish_task(&fixture.fixture.commit(ScopedBenchAction::RemoveOptional));
            assert_eq!(futures_executor::block_on(fixture.observation.next()), None);
            fixture
        });
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5
)]
fn scoped_handle_construction(bencher: divan::Bencher<'_, '_>, count: usize) {
    let fixture = scoped_bench_fixture();
    bencher.counter(count).bench_local(|| {
        futures_executor::block_on(async {
            let mut handles = Vec::with_capacity(count);
            for _ in 0..count {
                handles.push(optional_scoped_handle(&fixture.store).await);
            }
            divan::black_box(handles)
        })
    });
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5
)]
fn scoped_observation_construction(bencher: divan::Bencher<'_, '_>, count: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    bencher.counter(count).bench_local(|| {
        futures_executor::block_on(async {
            let mut observations = Vec::with_capacity(count);
            for _ in 0..count {
                observations.push(
                    handle
                        .observe(|child| child.value)
                        .await
                        .expect("observation registers"),
                );
            }
            divan::black_box(observations)
        })
    });
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5
)]
fn scoped_observation_coalescing(bencher: divan::Bencher<'_, '_>, commits: usize) {
    bencher
        .counter(commits)
        .with_inputs(observation_coalescing_fixture)
        .bench_local_values(|mut fixture| {
            for _ in 0..commits {
                finish_task(&fixture.fixture.commit(ScopedBenchAction::ParentOnly));
            }
            assert_eq!(
                futures_executor::block_on(fixture.observation.next()),
                Some(0),
                "one retained latest slot remains readable"
            );
            fixture
        });
}

#[divan::bench(args = [0_usize, 1, 4, 16], sample_count = 50, max_time = 0.5)]
fn dependency_lookup(bencher: divan::Bencher<'_, '_>, depth: usize) {
    match depth {
        0 => dependency_bench(bencher, DependencyBenchReducer),
        1 => dependency_bench(bencher, dependency_level(DependencyBenchReducer)),
        4 => dependency_bench(bencher, dependency_depth_4()),
        16 => dependency_bench(bencher, dependency_depth_16()),
        _ => unreachable!("Divan supplies only supported dependency depths"),
    }
}
