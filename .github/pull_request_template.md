## Change and reason

Describe the problem, the affected workload and the change.
If an issue or accepted design exists, link it here.

## Contracts and compatibility

Describe effects on public APIs, ownership, cancellation, ordering and shutdown.
State any feature, platform, dependency or migration effects.
For performance-sensitive changes, use the [qualification guide](../docs/maintainers/qualification.md#choose-performance-evidence-for-the-change).
Report the results that apply to this change:

- All measurements: name the workload, metric and revisions. Give the result, any trade-off and any inconclusive measurement.
- New feature: compare the existing workload with the feature off and on. Report the cost of using the new behavior.
- Compiler upgrade: compare unchanged source on both compilers. Then compare both revisions on the new compiler.

If a category does not apply, explain why.

## Checks and review

List exact commands, configurations and results. State which checks did not run and why.
For bug fixes, show which test failed before the fix and passed afterward.
A successful build does not prove that the code ran. Before you reuse earlier results, check their inputs.

- [ ] I followed the contribution and review guidance in `CONTRIBUTING.md`.
- [ ] I checked affected documentation, examples, agent skills and compatibility notes. For a deprecation, I documented the replacement and migration.
- [ ] I ran the applicable formatter, lint, test and contract checks.
- [ ] I recorded remaining risks and required checks, including independent review for high-risk changes.

These checks do not replace the release coverage, performance, platform or packaging gates.
Do not include credentials or security-sensitive reproduction details in this pull request.
