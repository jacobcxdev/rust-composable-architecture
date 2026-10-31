# Working on RCA

Read [CONTRIBUTING.md](CONTRIBUTING.md) before changing this repository.
Use its formatting, documentation, testing and review requirements.
Use the [authoring guide](docs/guides/authoring.md) for application design.
Use the [support policy](docs/support.md) for feature and platform boundaries.

## Design and scope

- Keep RCA headless-first. Host applications own UI integration and their runtime.
- Preserve ownership, ordering, cancellation, shutdown and child-identity contracts.
- Prefer the simplest design that preserves required behaviour and measured costs.
- Keep changes focused. Preserve unrelated work and existing licence notices.
- Do not add dependencies or abstractions without a concrete use case.

## Evidence and review

- Select the pinned Rust tools explicitly with `rustup run 1.99.0`.
- For local macOS builds, run repository compilation through `scripts/guard_cargo_memory.py`. Follow the memory-safety rules in CONTRIBUTING.
- Do not bypass a memory abort or run controllers with unguarded compiler process groups. Use OS-limited remote jobs while resolving the cause.
- Start with focused tests. Then check affected integration boundaries.
- Use the recursive review questions in CONTRIBUTING for each affected source unit.
- Keep executable production coverage separate from test-only source review.
- Reuse earlier results only when their source, configuration and environment remain applicable.
- Record missing checks and known failures. Do not describe compilation as runtime proof.
- Keep private research, credentials, build output and temporary agent records out of commits.
- Use signed commits and the pull-request template. Do not force-push shared history.
- Treat code review, merge, package publication and documentation publication as separate actions.
