# Documentation publication

The documentation site shows the API at each branch and tag. It updates automatically. Publishing documentation does not publish crates.

| Ref | Documentation on the site |
| --- | --- |
| `main` | Stable API reference |
| `dev/next` | Development API reference |
| Any other branch | Preview of that branch |
| Any tag | Reference for that tag, separate from a branch of the same name |

## What happens after a push

The [push workflow](../.github/workflows/docs-push.yml) notifies the [publisher](../.github/workflows/publish-docs.yml) on the default branch.
The publisher checks the live Git ref and builds that exact commit. It ignores a notification for a commit that the ref no longer names.
A separate job downloads the build from the same run, checks its digest, and signs the documentation update.

A deletion removes only that branch or tag from the site. A delayed deletion does not remove a ref that someone recreated.
A pull request from a fork does not publish to this site. Crate releases have a [separate workflow](releasing.md).

Old branches can lack the push workflow. GitHub can also omit events for bulk tag changes.
A daily check compares the live refs with the published refs and requests repairs for missing, old, or deleted references.
If a commit cannot build the API reference, the run fails. The publisher does not label an older build as current.

## Check a build

In GitHub Actions, open the publication run for the commit. Download the `rca-api-reference-*` archive within seven days.
Extract the archive. Open `index.html`.

The [build tool](../scripts/build_doc_preview.py) uses a pinned Rust image and a fresh output directory.
It builds the published libraries with all features and treats warnings as errors. It does not publish examples or test programs.
The source build has no signing key, site token, or shared compiler cache.

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

## Configure automatic publication

1. Protect the default branch. Require reviewed workflow changes and signed commits.
2. Configure the `documentation` environment to accept only that branch. Do not allow tag deployments or administrator bypass.
3. Register the dedicated public signing key. Check its fingerprint against the publisher.
4. Set `DOCS_SIGNING_PRIVATE_KEY`, `DOCS_SIGNING_PASSPHRASE`, and `INDEX_REPO_TOKEN` in that environment.
   Give the fine-grained token contents read and write access to the documentation index only.
5. Keep required reviewers while testing the credentials. Run **Publish Documentation** from the default branch for the default branch with **Check credentials and sign a local commit without publishing** selected.
   Approve that run. Confirm that it checks write access and signs a local commit without pushing.
6. Remove any repository-level copies of the site credentials after the check passes. Confirm the environment still has its own credentials before removing the required reviewers.
   Publication then runs without manual approval.

The signing job runs on a fresh runner. It uses only trusted publication code from the default branch and the pinned site publisher.
The private key enters only the import step. The GPG wrapper passes the passphrase through a separate file descriptor.
The workflow checks the fingerprint before signing. It stops the GPG agent and removes the temporary keyring after publication.

The dedicated key supports separate revocation. It can sign commits outside RCA under its registered GitHub account.
Keep recovery copies outside the checkout. If the key is compromised, revoke it and replace it before publication resumes.

## Repair one ref

After a failed build or site deployment, open **Publish Documentation** in GitHub Actions.
Select the default branch as the workflow source. Enter the exact branch or tag name and its kind.
The repair checks the live ref again. It does not accept an archive or commit supplied by the caller.

Check that `docs-meta.json` on the published page names the source commit.
A successful local build does not prove that the site deployed.
