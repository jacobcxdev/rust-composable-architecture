# Packaged representative examples

These standalone manifests qualify the unchanged sources under `examples/`
against the temporary registry created by `scripts/verify-packaged-downstream.sh`.
They are staging inputs, not standalone source checkouts. The verifier copies
each example's `src` and any integration tests into its temporary consumer.

Keep dependency features aligned with the corresponding example. The composition
manifest combines its normal and development RCA feature sets so the existing
registry-only fixture validator can validate one renamed umbrella dependency.
No application source is duplicated here. Each consumer is checked, tested and
run offline after its dependencies have been fetched.
