# Mounted keyed allocation diagnostic

This diagnostic measures current RCA only. Upstream Composable at the pinned
revision has no native keyed reducer API. An application-written adapter would
measure different work, so it is not presented as an equivalent baseline.

The [fixture guide](../../tests/composable-keyed-policy/README.md#mounted-allocation-diagnostics)
defines the exact trace, state checks, child-release checks and allocation
boundaries. The former fork comparison and its development results are archived;
they are not current release evidence.

## Work and boundaries

Build the candidate fixture with pinned Rust 1.98.0, locked dependencies and the
release profile. Its DHAT dependency is always enabled. Run each cardinality (0, 2, 10
and 1,000) in a separate process. Keep the fixture's external process deadline.
A failed trace, state or ownership check must not produce a successful ledger.

The ten snapshots separate collection creation, population, mounting, child
work, removal, reinsertion, clearing, extraction and final domain release.
The empty control exposes fixed host and Store costs. Removing children drops
their owned state, but retained collection capacity is not automatically a leak.
Subtracting an empty control may help investigate costs; it does not prove that
allocator costs add independently.

DHAT counts all process allocations, including host and fixture coordination.
Keep cumulative allocated blocks/bytes separate from current live blocks/bytes.
DHAT's `max_blocks` is the block count at peak bytes, not an independent peak
block count.

## Interpretation

Action completion is not heap quiescence. Runtime bookkeeping may overlap a
snapshot. Domain child/root destruction checks do not require unrelated global
or runtime allocations to reach zero. Use allocation stacks to investigate
retention before changing capacity policy or calling it a leak.

The output retains `comparison_qualified: false`. Successful fixture checks
establish only the stated finite workload and ownership boundaries. Allocation
observations do not establish timing, statistical confidence, equivalence to a
different architecture or general performance superiority. Record exact source,
toolchain, feature, allocator and host identities with any new measurements.
