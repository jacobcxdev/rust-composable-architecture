## Change and reason

Describe the problem, the affected workload and the change.
Link the related issue or accepted design, where applicable.

## Contracts and compatibility

Describe effects on public APIs, ownership, cancellation, ordering and shutdown.
State any feature, platform, dependency or migration effects.
For measured cost changes, include the workload, method and before/after results.
If a category does not apply, explain why.

## Checks and review

List exact commands, configurations and results. State which checks did not run and why.
For bug fixes, include the failing regression and the result after the repair.
Do not treat compilation as runtime proof or earlier results as current without checking their inputs.

- [ ] I followed the contribution and review guidance in `CONTRIBUTING.md`.
- [ ] I checked affected documentation, examples and compatibility notes.
- [ ] I ran the applicable formatter, lint, test and contract checks.
- [ ] I recorded remaining risks and required checks, including independent review for high-risk changes.

These checks do not replace the release coverage, performance, platform or packaging gates.
Do not include credentials or security-sensitive reproduction details in this pull request.
