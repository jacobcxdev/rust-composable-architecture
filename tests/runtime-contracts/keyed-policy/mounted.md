# Measure allocations for mounted children

This fixture checks what the Store retains as it adds and removes children. It measures a finite workload, not a release speedup.

Run the [keyed fixture](README.md) once per child count: `0`, `2`, `10`, and `1000`.
On macOS, start at the repository root and use the [memory guard](../../../CONTRIBUTING.md#build-memory-safety):

```sh
python3 scripts/guard_cargo_memory.py --receipt "$(mktemp -d)/mounted.json" -- \
  run --release --locked --manifest-path tests/runtime-contracts/keyed-policy/candidate/Cargo.toml \
  --bin mounted-allocations -- 2
```

On an isolated runner with an OS memory limit, use `rustup run 1.100.0 cargo` with the same `run` arguments.
The fixture uses DHAT to count process allocations. Its ten snapshots cover collection creation, insertion, mounting, child work, removal, replacement, clearing, extraction, and final release.

The empty case shows fixed costs. Subtracting it does not prove that allocator costs add independently.
Compare allocated blocks and bytes with the **live** blocks and bytes at each snapshot.
DHAT reports `max_blocks` at the point of peak bytes. It does not report the independent peak count of blocks.

A completed action does not mean all runtime allocation has stopped. Freed children can leave reusable capacity in the collection.
Use the child and root drop checks to test ownership. Use allocation stacks to investigate unexpected retention.
A failed state, ownership, or deadline check invalidates the result.

Record the measured source, toolchain, allocator, features, and host with the results. Do not commit generated output.
