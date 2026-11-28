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
The `crates/rca` directory contains the public facade. Code that works with any runtime belongs in `crates/rca-core`.
The Tokio adapter and test driver have separate crates under `crates/`. The transport forks live in `dependencies/`.
Before changing a bound, check both shared and owner-thread APIs. Do not require `Send` or `Clone` from callers unless the public contract needs it.
Before changing a receipt, distinguish mailbox admission, committed state, completion of tracked work, and acknowledgement by an external service.

Choose the smallest test that distinguishes the proposed behavior from the old behavior:

| Change | First check |
| --- | --- |
| Keyed child lifetime | Remove a child while a reply is pending. Reuse its ID. Reject the old reply and accept the new one. See `crates/rca-test/tests/shared_keyed_composition.rs`. |
| Cancellation inheritance | Hold the child effect pending. Cancel its inherited or explicit key, then release it. See `crates/rca-core/tests/cancellation_handoff.rs`. |
| A borrow or trait bound | Pair a passing caller with a caller that fails at the intended operation. Use the owning suite in `docs/development/compiler-contract-testing.md`. |
| A warning or deprecation | Keep the old caller compiling with the intended warning. Check that its replacement compiles without the warning. |
| A public path, feature flag, or macro | Build an external consumer with the smallest affected feature set. For facade exports or macro paths, check each affected package with its default import and a Cargo alias. |
| A runtime adapter | Run its conformance suite. In `crates/rca-tokio/tests/async_cleanup_host.rs`, the peer checks that the local socket closed. This does not prove that a remote service released its resource. |
| A guide or skill | Exercise its example or decision from the reader's position. Check the links in the distributed form, not just the source tree. |

For a Markdown change, use the “How CI selects checks” section of `CONTRIBUTING.md`.
A crate's package page needs a package check. If a tool reads Markdown as test data, mark the input with `rca-ci-input` in `.gitattributes`.

On macOS, use the repository's Cargo memory watchdog before compiling. If it stops a build, inspect its receipt. Do not bypass it.
After the focused check, test the affected integration boundaries.
For a performance change, choose a public workload from `docs/development/benchmarking.md` that exercises the affected path. Compare the same completed work, including cleanup.
An allocation count alone does not establish lower latency. See `docs/development/qualification.md` for the release gates.
Keep runner profiles and raw measurements out of the public repository. Without the required profile, report the missing release check. Do not substitute a local timing.

## Keep guidance with the contract

If a public contract changes, update its rustdoc, guide or example, and the affected agent instructions together.
For a new derive, show the caller which code it replaces and when to use the manual form.
For a deprecation, state the replacement and any change in behavior. Record migration steps in the changelog.
Use `docs/support.md` for compatibility policy and `docs/development/releasing.md` for the release review.
A passing PR does not authorize a tag or publication.
