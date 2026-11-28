---
name: rca-contribute
description: Change or review Rust Composable Architecture itself. Use for RCA library code, compiler fixtures, public guides, skills, and qualification tooling in the RCA repository. Not for building an application that depends on RCA.
license: MIT OR Apache-2.0
---

# Contribute to RCA

Work from the RCA checkout under review. Read its `AGENTS.md` and `CONTRIBUTING.md`. Paths below are relative to that checkout.
Use its toolchain, lockfiles, and documentation. A skill from another release does not override them.

## Start at the caller

Name the behavior a caller can observe and find the test that demonstrates it.
The `rca` directory contains the facade. Runtime-neutral behavior belongs in `rca-core`. The Tokio adapter and test driver have separate crates.
Before changing a bound, check both shared and owner-thread APIs. Do not require `Send` or `Clone` from callers unless the public contract needs it.
Before changing a receipt, distinguish mailbox admission, committed state, completion of tracked work, and acknowledgement by an external service.

Choose the smallest test that distinguishes the proposed behavior from the old behavior:

| Change | First check |
| --- | --- |
| Keyed child lifetime | Remove a child while a reply is pending. Reuse its ID. Reject the old reply and accept the new one. See `rca-test/tests/shared_keyed_composition.rs`. |
| Cancellation inheritance | Hold the child effect pending. Cancel its inherited or explicit key, then release it. See `rca-core/tests/cancellation_handoff.rs`. |
| A borrow or trait bound | Pair a passing caller with a caller that fails at the intended operation. Use the owning suite in `docs/compiler-contract-testing.md`. |
| A warning or deprecation | Keep the old caller compiling with the intended warning. Check that its replacement compiles without the warning. |
| A public path, feature flag, or macro | Build an external consumer with the smallest affected feature set. For facade exports or macro paths, check each affected package with its default import and a Cargo alias. |
| A runtime adapter | Run its conformance suite. For host-owned cleanup after Store shutdown, see `rca-tokio/tests/async_cleanup_host.rs`. That test does not prove external resource release. |
| A guide or skill | Exercise its example or decision from the reader's position. Check the links in the distributed form, not just the source tree. |

On macOS, use the repository's Cargo memory watchdog before compiling. If it stops a build, inspect its receipt. Do not bypass it.
After the focused check, test the affected integration boundaries. See `docs/qualification.md` for performance and release checks.
For an optimization, compare the same completed work and include cleanup. An allocation count alone does not establish lower latency.

## Keep guidance with the contract

If a public contract changes, update its rustdoc, guide or example, and the affected agent instructions together.
For a new derive, show the caller which code it replaces and when to use the manual form.
For a deprecation, state the replacement and any change in behavior. Record migration steps in the changelog.
Use `docs/support.md` for compatibility policy and `docs/releasing.md` for the release review.
A passing PR does not authorize a tag or publication.
