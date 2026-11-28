//! Measures the real output request and inert removal metadata.
use std::hint::black_box;
use std::mem::{align_of, size_of};
use std::sync::Arc;

use super::{ScopeIdentity, ScopeNodeId, ScopeRegistry};
use crate::store::output::{OutputRequest, OutputSlot};

/// Measures request and receiver ownership separately, excluding caller payload construction.
fn request_sample<O>(
    value: O,
    #[cfg(feature = "test-util")] origin: Option<crate::RemovalOrigin>,
) -> [dhat::HeapStats; 4] {
    let before = dhat::HeapStats::get();
    let (request, receive) = black_box(OutputRequest::channel(
        value,
        #[cfg(feature = "test-util")]
        origin,
    ));
    let created = dhat::HeapStats::get();
    drop(black_box(request));
    let request_dropped = dhat::HeapStats::get();
    drop(black_box(receive));
    let receive_dropped = dhat::HeapStats::get();
    assert_eq!(created.total_blocks - before.total_blocks, 2);
    assert_eq!(created.curr_blocks - before.curr_blocks, 2);
    assert_eq!(request_dropped.curr_blocks - before.curr_blocks, 1);
    assert_eq!(receive_dropped.total_blocks, created.total_blocks);
    assert_eq!(receive_dropped.total_bytes, created.total_bytes);
    assert_eq!(receive_dropped.curr_blocks, before.curr_blocks);
    assert_eq!(receive_dropped.curr_bytes, before.curr_bytes);
    [before, created, request_dropped, receive_dropped]
}

/// Prints allocation deltas and retained storage for each measured ownership boundary.
fn report_request(label: &str, origin: &str, stats: &[dhat::HeapStats; 4]) {
    for (phase, pair) in ["create", "request_drop", "receiver_drop"]
        .into_iter()
        .zip(stats.windows(2))
    {
        eprintln!(
            "output_request_cost payload={label} origin={origin} phase={phase} events={} requested_bytes={} live_blocks_before={} live_blocks_after={} live_bytes_before={} live_bytes_after={}",
            pair[1].total_blocks - pair[0].total_blocks,
            pair[1].total_bytes - pair[0].total_bytes,
            pair[0].curr_blocks - stats[0].curr_blocks,
            pair[1].curr_blocks - stats[0].curr_blocks,
            pair[0].curr_bytes - stats[0].curr_bytes,
            pair[1].curr_bytes - stats[0].curr_bytes,
        );
    }
}

/// Checks feature-off layout and reports metadata storage for each concrete payload.
fn layout<O>(label: &str) {
    #[cfg(not(feature = "test-util"))]
    assert_eq!(
        size_of::<OutputSlot<O>>(),
        size_of::<parking_lot::Mutex<Option<O>>>(),
        "ordinary output slots must not retain test-only origin storage"
    );
    #[cfg(not(feature = "test-util"))]
    assert_eq!(
        align_of::<OutputSlot<O>>(),
        align_of::<parking_lot::Mutex<Option<O>>>()
    );
    eprintln!(
        "output_slot_layout payload={label} current_size={} current_align={} historical_value_component_size={} historical_value_component_align={} inline_delta={} origin_size={} optional_origin_size={}",
        size_of::<OutputSlot<O>>(),
        align_of::<OutputSlot<O>>(),
        size_of::<parking_lot::Mutex<Option<O>>>(),
        align_of::<parking_lot::Mutex<Option<O>>>(),
        size_of::<OutputSlot<O>>() - size_of::<parking_lot::Mutex<Option<O>>>(),
        size_of::<crate::RemovalOrigin>(),
        size_of::<Option<crate::RemovalOrigin>>(),
    );
}

/// Measures inert origin capture and proves it retains neither child nor lifecycle owners.
#[expect(
    clippy::too_many_lines,
    reason = "keep output origin construction, admission and retirement measurements together"
)]
pub(crate) fn output_origin_costs() {
    const NAME: &str = "store::scope::output_origin_costs::output_origin_costs";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let (lifecycle, mut registry) = ScopeRegistry::for_test();
    let node = ScopeNodeId::new();
    let node_weak = Arc::downgrade(&node.0);
    let child = registry.mount(registry.root(), node, ScopeIdentity::unit());
    let root = registry.root_identity();
    let root_weak = Arc::downgrade(&root);
    let lifecycle_weak = Arc::downgrade(&lifecycle);
    let before_nodes = node_weak.strong_count();
    let before_root = Arc::strong_count(&root);
    let before = dhat::HeapStats::get();
    let origin = black_box(registry.removal_origin(child));
    let captured = dhat::HeapStats::get();
    let captured_root = Arc::strong_count(&root);
    let clone = black_box(origin.clone());
    let cloned = dhat::HeapStats::get();
    let cloned_root = Arc::strong_count(&root);
    assert_eq!(captured.total_blocks, before.total_blocks);
    assert_eq!(captured.total_bytes, before.total_bytes);
    assert_eq!(cloned.total_blocks, captured.total_blocks);
    assert_eq!(cloned.total_bytes, captured.total_bytes);
    assert_eq!(captured_root, before_root + 1);
    assert_eq!(cloned_root, before_root + 2);
    assert_eq!(node_weak.strong_count(), before_nodes);
    assert_eq!(origin, clone);
    eprintln!(
        "output_origin_cost capture_events={} capture_requested_bytes={} clone_events={} clone_requested_bytes={} root_owners={before_root},{captured_root},{cloned_root} child_node_owners={before_nodes}",
        captured.total_blocks - before.total_blocks,
        captured.total_bytes - before.total_bytes,
        cloned.total_blocks - captured.total_blocks,
        cloned.total_bytes - captured.total_bytes
    );

    #[cfg(feature = "test-util")]
    for (label, none, some) in [
        (
            "unit",
            request_sample((), None),
            request_sample((), Some(origin.clone())),
        ),
        (
            "u64",
            request_sample(7_u64, None),
            request_sample(7_u64, Some(origin.clone())),
        ),
        (
            "bytes64",
            request_sample([7_u8; 64], None),
            request_sample([7_u8; 64], Some(origin.clone())),
        ),
    ] {
        assert_eq!(
            none[1].total_blocks - none[0].total_blocks,
            some[1].total_blocks - some[0].total_blocks
        );
        assert_eq!(
            none[1].total_bytes - none[0].total_bytes,
            some[1].total_bytes - some[0].total_bytes
        );
        report_request(label, "None", &none);
        report_request(label, "Some", &some);
    }
    #[cfg(not(feature = "test-util"))]
    for (label, stats) in [
        ("unit", request_sample(())),
        ("u64", request_sample(7_u64)),
        ("bytes64", request_sample([7_u8; 64])),
    ] {
        report_request(label, "feature-off", &stats);
    }
    assert_eq!(Arc::strong_count(&root), cloned_root);
    drop(origin);
    assert_eq!(Arc::strong_count(&root), before_root + 1);
    drop(registry);
    drop(lifecycle);
    drop(root);
    assert_eq!(root_weak.strong_count(), 1);
    assert_eq!(
        node_weak.strong_count(),
        0,
        "origin must not retain the retired child node"
    );
    assert_eq!(
        lifecycle_weak.strong_count(),
        0,
        "origin must not retain the lifecycle owner"
    );
    drop(clone);
    assert_eq!(root_weak.strong_count(), 0);
    eprintln!(
        "output_origin_retention after_registry_drop_root_owners=1 child_node_owners=0 lifecycle_owners=0 after_final_origin_drop_root_owners=0"
    );
    layout::<()>("unit");
    layout::<u64>("u64");
    layout::<[u8; 64]>("bytes64");
    layout::<Box<u64>>("box_u64");
    drop((root_weak, node_weak, lifecycle_weak));
    drop(profiler);
}
