# Packaged representative examples

Use these manifests to check the examples against packaged RCA crates.
The example code lives under `examples/`. The verifier,
`scripts/verify-packaged-downstream.sh`, creates a temporary registry, then
copies each example's `src` and any integration tests alongside its manifest.
The manifests are not runnable examples on their own.

Keep dependency features aligned with the corresponding example. The composition
manifest combines the RCA features needed by the app and its tests. This lets
the verifier check one dependency on the public RCA crate under its local name.
After fetching dependencies, the verifier checks, tests and runs each example
offline.
