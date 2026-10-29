# RCA 0.1.0 qualification

RCA 0.1.0 is an unpublished candidate. This document separates local qualification,
hosted CI and publication. None of these establishes that software has no defects.

## Recorded local results

Local release-readiness qualification completed on 29 October 2026 for the recorded
source candidate. The retained source envelope identifies it as
`58e860271a7a88b079274541956ddd5100244c69c5dc4b9399bdff6aadbf8cd2`.
This identifier is a source-record identifier, not a public Git commit.

The recorded results include:

- Workspace tests, strict Clippy checks and compiler-contract tests.
- 236 workspace feature configurations and 128 facade test configurations.
- Public and private documentation checks, 204 documentation tests and rendered-link checks.
- 17 property tests with 10,000 cases each, plus transport and concurrency checks.
- 25 strict-provenance Miri checks and four Tokio conformance checks.
- Nine package archives and 13 consumers built from those archives.
- Performance checks with workload-specific allocation, instruction-count and native evidence.

The executable-production coverage record accounts for 5,131 bodies, 49,080 regions
and 3,432 branch sides. Native coverage covers 5,050 bodies.
Complete finite-operation checks account for 81 additional bodies.
Test-only code, compile-failure fixtures and conformance-test bodies have a separate audit scope.
This is not a claim that every test-only failure path executed.

The retained coverage result has SHA-256
`fed723d58529b722e2df99c8a9d2ff98150b9c5d2ead1f74fececa096ecaf912`.
The retained release-gate record has SHA-256
`989b1a282734f1a3b89970d478d25ffa89d1896673faad0542b2451c9a78d9d7`.
These hashes identify local records. They do not substitute for accessible logs or independent reproduction.

## Public snapshot boundary

The initial public snapshot preserves the implementation from the qualified candidate.
Publication preparation updates repository guidance, metadata checks and the explicit
nightly selection in the Miri workflow. It does not change executable Rust implementation.

The README examples retain their qualified source and manifests.
README prose changes mean that earlier package archives are not byte-identical to
packages produced from the public snapshot. Earlier results must not be presented
as exact-archive verification of later package bytes.

Raw profiles, temporary agent records and private upstream research remain outside
the public repository. Historical local audit receipts are not public reproduction
instructions. Current tests, required regression inputs and maintained development
tools remain part of the snapshot.

## Hosted checks and publication

Hosted CI is a separate requirement. A local result does not establish that GitHub
Actions passed for the public commit. The pull request records its current CI status.
See the [support policy](support.md#qualification-status) for the target matrix.

The initial review does not publish packages, create a release tag or publish the
documentation site. Documentation publication requires its protected environment
and signing configuration. See [documentation publication](documentation-publishing.md).

Before release, package verification must cover the exact release source and archive bytes.
Any remaining hosted or publication check must retain its actual status.

## Repeating affected checks

Use [CONTRIBUTING.md](../CONTRIBUTING.md) for toolchain, formatting, lint and test commands.
Use [CI](../.github/workflows/ci.yml) for feature, platform and compiler-contract configurations.
Use [the compiler-contract guide](compiler-contract-testing.md) for compile-time checks.
The Tokio conformance tests live in `rca-tokio/src/lib.rs`.
The package-consumer runner is [verify-packaged-downstream.sh](../scripts/verify-packaged-downstream.sh).

Performance comparisons need matching workloads and applicable measurement conditions.
The pinned upstream Composable stream reference can hang during shutdown.
It remains a failed reference baseline, not an RCA failure or a valid numerical comparison.
RCA's own correctness, coverage and performance requirements remain unchanged.
See [the stream fixture](../tests/composable-stream/README.md) for that boundary.

Older performance instructions can name unpublished development revisions.
Those comparisons require the retained local history and cannot run from a fresh
public clone without the specified baseline. The current public legacy fixtures
pin the preserved Composable revision `c6200234cebbbd9b0009cd5788736c78a47f32b0`.
