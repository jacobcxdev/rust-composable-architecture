# Documentation publication

The publication workflow builds the workspace documentation with the pinned Rust
toolchain, all features and warnings denied. It first builds documentation for
`futures-core` and the owned event-listener fork. These separate builds supply the
metadata for inherited and re-exported API links. It then builds the workspace
without other dependency documentation. The private index repository holds
the published pages. `main` selects the stable channel.
Tags that match `v*` retain separate paths. No development branch is published.

Both CI workflows use this build order:

```sh
export RUSTDOCFLAGS='-D warnings'
rustup run 1.98.0 cargo doc --locked -p futures-core --no-deps
rustup run 1.98.0 cargo doc --locked -p rca-event-listener --all-features --no-deps
rustup run 1.98.0 cargo doc --locked --workspace --all-features --no-deps
```

The two prerequisite commands stay separate because Cargo 1.98 cannot resolve
the combined package selection in this workspace.

## Signing identity

Publication requires the dedicated `RCA Documentation CI <me@jacobcx.dev>` identity.
Its OpenPGP fingerprint is:

```text
7237 B7C9 DE66 9B33 CF0B 07A8 F7E4 F884 6931 6E38
```

The Ed25519 key expires on 26 October 2027. The primary key has signing and
certification capabilities. It has no encryption or authentication capability.
The workflow pins the full fingerprint and checks a signature before publication.
Both the author and committer use this identity.

The protected environment and both signing secrets are configured. GitHub holds
the public key under the owner's account. The registered fingerprint matches the
workflow. The first hosted publication with this signer remains unverified.
Local checks do not establish a verified GitHub commit or successful hosted publication.

## Secret boundary

The `documentation` environment holds `DOCS_SIGNING_PRIVATE_KEY` and
`DOCS_SIGNING_PASSPHRASE`. The existing `INDEX_REPO_TOKEN` grants access to the
private index repository. No secret value belongs in this repository.

The private key enters only the import step. The passphrase enters only the
signing and publication steps. A small GPG wrapper supplies the passphrase through
a separate file descriptor. It does not expose the passphrase in command arguments
or replace the commit payload on standard input.

Each run uses a temporary GPG home. An unconditional cleanup step stops that
home's agent and removes its files. It refuses paths outside the expected runner
directory. Abrupt runner termination can prevent cleanup, so publication uses
an ephemeral GitHub-hosted runner.

A dedicated key supports separate revocation. It does not restrict signatures to
RCA. A compromised key can sign other commits under the registered GitHub account.
The environment requires approval from `jacobcxdev`, with administrator bypass
disabled. It accepts only `main` and `v*` tags. Self-review remains
available so that the owner can approve runs from the owner's own pushes.

## First publication requires owner approval

1. Check the public key fingerprint against this document and the workflow.
2. Check the protection and both signing secrets in the `documentation` environment.
3. Check access to the private action and index repository.
4. Obtain approval for the first live publication.
5. Check the resulting commit signature, channel path, licences and hosted pages.

The owner approved key registration, environment protection and secret configuration.
That approval does not authorise code pushes or publication.
The initial private key and provisioning passphrase remain in an owner-only
directory outside the checkout. Because they are adjacent, encryption does not
provide independent protection against access to that directory.

Before key expiry, prepare and approve a replacement identity. Update the pinned
fingerprint and secrets together. Check a signed publication before revoking the
old key. If compromise is suspected, revoke the old key immediately and stop
publication until a replacement passes the required checks.
