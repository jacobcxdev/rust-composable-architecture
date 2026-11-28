# Working on RCA

Read [CONTRIBUTING.md](CONTRIBUTING.md) before changing this repository.
Use its formatting, documentation, testing and review requirements.
Use the [authoring guide](docs/usage/authoring.md) for application design.
Use the [support policy](docs/support.md) for feature and platform boundaries.

## Design and scope

- Keep RCA headless-first. Host applications own UI integration and their runtime.
- Preserve ownership, ordering, cancellation, shutdown and child-identity contracts.
- Prefer the simplest design that preserves required behaviour and measured costs.
- Keep changes focused. Preserve unrelated work and existing licence notices.
- Do not add dependencies or abstractions without a concrete use case.

## Evidence and review

- Select the pinned Rust tools explicitly with `rustup run 1.100.0`.
- On macOS, run local builds through `scripts/guard_cargo_memory.py`. See [build memory safety](CONTRIBUTING.md#build-memory-safety).
- If the watchdog stops a build, do not bypass it. Use a remote job with an OS memory limit until the cause is clear.
- Do not run a local controller that starts unguarded compiler process groups.
- Start with focused tests. Then check affected integration boundaries.
- Use the [review questions](CONTRIBUTING.md#reviewing-a-change) for each affected source unit.
- Keep executable production coverage separate from test-only source review.
- Reuse earlier results only when their source, configuration and environment remain applicable.
- Record missing checks and known failures. Do not describe compilation as runtime proof.
- Keep private research, credentials, build output and temporary agent records out of commits.
- Use signed commits and the pull-request template. Do not force-push shared history.
- Treat code review, merge, package publication and documentation publication as separate actions.
