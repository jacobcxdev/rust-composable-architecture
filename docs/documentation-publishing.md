# Documentation publication

The [documentation workflow](../.github/workflows/publish-docs.yml) sends the API reference to the documentation index after maintainer approval.

| Push | Public documentation |
| --- | --- |
| `main` | Stable API reference |
| `dev/next` | Development API reference. These APIs can change before release. |
| A tag that matches `v*` | A separate reference built from that tag. Later branch changes do not update it. |

Pull requests, other branches, and branch or tag deletions do not publish documentation.
The index keeps both branch names in `branch-designations.json`, including on tag pushes.
Publishing documentation does not publish crates. Crate releases have a [separate tag check and approval](releasing.md).

The documentation workflow uses the same compiler as [rust-toolchain.toml](../rust-toolchain.toml).
It builds every published library with all features and treats warnings as errors.
Examples and test programs do not appear in the API reference.
On macOS, use the [build memory safeguards](../CONTRIBUTING.md#build-memory-safety) for these builds.
On an isolated runner, use rustup's Cargo from the RCA checkout:

```sh
export RUSTDOCFLAGS='-D warnings'
cargo doc --locked -p futures-core --no-deps
cargo doc --locked -p rca-event-listener --all-features --no-deps
cargo doc --locked --workspace --lib --exclude 'rca-example-*' --exclude rca-size-probe --all-features --no-deps
python3 scripts/prepare_public_docs.py target/doc
```

Keep the first two commands separate. They supply the metadata for links to
inherited and re-exported APIs before rustdoc builds the workspace.

The last command checks the list of crate pages, prepares the site root, and copies both licences.
Each source page names the published file, not its location on the build machine.
The command stops publication if it finds a build path in the output.

## Signing identity

The workflow defines the documentation signing identity and pins its full OpenPGP fingerprint.
Both the author and committer use this identity. The workflow checks a signature before publication.

Register the public key under the maintainer's GitHub account. Check that its
fingerprint matches the workflow. A successful publication requires a valid
commit signature and accessible hosted pages. A successful local build proves neither.

## Secret boundary

The `documentation` environment holds `DOCS_SIGNING_PRIVATE_KEY` and
`DOCS_SIGNING_PASSPHRASE`. The `INDEX_REPO_TOKEN` grants access to the
documentation index. No secret value belongs in this repository.

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
The environment requires maintainer approval, with administrator bypass
disabled. Configure it to accept only the branches `main` and `dev/next` and tags that match `v*`. Self-review remains
available so that the owner can approve runs from the owner's own pushes.

## Publish documentation

1. Check the public key fingerprint against the workflow.
2. Check the protection and both signing secrets in the `documentation` environment.
3. Check access to the publication action and the documentation index.
4. Before you approve the run, check its source commit and the workflow at that commit.
5. Check the signature on the new index commit and the result of the Pages deployment.
6. Open the published API reference and its licence files. In the same publication directory, check that `docs-meta.json` records the source commit.

Keep the same branch protection on `dev/next` and `main`.

Approval for key registration or secret configuration does not authorise publication.
Keep recovery copies of the private key and passphrase outside the checkout.
Storing them together does not provide independent protection against access to that location.

Before key expiry, prepare and approve a replacement identity. Update the pinned
fingerprint and secrets together. Check a signed publication before revoking the
old key. If compromise is suspected, revoke the old key immediately and stop
publication until a replacement passes the required checks.
