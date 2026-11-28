# Measure allocations for mounted children

This fixture checks what the Store retains as it adds and removes children. It measures a finite workload, not a release speedup.

Run the [keyed fixture](README.md) in separate processes with 0, 2, 10, and 1,000 children. Use the repository toolchain, the locked dependencies, and the release profile.
The fixture uses DHAT to count process allocations. Its ten snapshots cover collection creation, insertion, mounting, child work, removal, replacement, clearing, extraction, and final release.

The empty case shows fixed costs. Subtracting it does not prove that allocator costs add independently.
Compare allocated blocks and bytes with the **live** blocks and bytes at each snapshot.
DHAT reports `max_blocks` at the point of peak bytes. It does not report the independent peak count of blocks.

A completed action does not mean all runtime allocation has stopped. Freed children can leave reusable capacity in the collection.
Use the child and root drop checks to test ownership. Use allocation stacks to investigate unexpected retention.
A failed state, ownership, or deadline check invalidates the result.

Record the measured source, toolchain, allocator, features, and host with the results. Do not commit generated output.
