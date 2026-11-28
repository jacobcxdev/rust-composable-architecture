# Documentation publication

After configuration, the workflow publishes API documentation for branches and tags that build successfully.
The [documentation catalogue](https://docs.jacobcx.dev/) shows the last successfully published snapshot after site deployment.
Publishing documentation does not publish crates.

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

For a deletion, the publisher checks that the exact branch or tag no longer exists.
If the publisher finds the ref again, it skips the removal. It does not remove a different ref with the same name.
A pull request from a fork does not publish to this site. Crate releases have a [separate workflow](releasing.md).

Git and the site update separately. A ref can change again between the publisher's last check and its push.
If a build or deployment fails, an older snapshot can remain visible, or the page can be absent.
Old branches can lack the push workflow. GitHub can also omit events for bulk tag changes.
A daily check compares live refs with the [published RCA inventory](https://docs.jacobcx.dev/repo/1123147933/publications.json) and requests repairs for differences.

## Check a build

1. In GitHub Actions, open a **Publish Documentation** run. **Documentation Push** only sends the notification.
2. In **Checkout source at the verified commit**, check that the source commit matches the revision you want.
   The workflow itself runs from the default branch, which can name a different commit.
3. Download that run's `rca-api-reference-*` archive within seven days. Extract it. Open `index.html`.

A deletion or an ignored notification produces no build archive.

## Build a local preview

The [build tool](../scripts/build_doc_preview.py) uses a pinned Rust image and a fresh output directory.
It builds the published libraries with all features and treats warnings as errors. It does not publish examples or test programs.
The source build has no signing key, site token, or shared compiler cache.

For a historical revision, use Docker on an isolated Linux runner.
Set `source_checkout` to the checkout of the revision you want to read.
Set `output` to a path outside that checkout. Its parent must exist, but the output directory must not.
From the checkout that contains the trusted build tool, run:

```sh
python3 scripts/build_doc_preview.py "$source_checkout" "$output" --revision "$(git -C "$source_checkout" rev-parse HEAD)"
```

The tool selects packages from the source revision. For example, the `v0.0.1` preview has only two RCA libraries.
After the command succeeds, open `index.html` in the output directory.

The commands below are for the **current checkout** only. They use the repository's pinned Cargo in an isolated runner.
On macOS, run each Cargo command through the [memory watchdog](../CONTRIBUTING.md#build-memory-safety) instead.

```sh
export RCA_DOCS_REVISION="$(git rev-parse HEAD)"
export RUSTDOCFLAGS='-D warnings --cfg rca_docs_source'
rustup run 1.100.0 cargo doc --locked -p futures-core --no-deps
rustup run 1.100.0 cargo doc --locked -p rca-event-listener --all-features --no-deps
rustup run 1.100.0 cargo doc --locked --workspace --lib --exclude 'rca-example-*' --exclude rca-size-probe --all-features --no-deps
python3 scripts/prepare_public_docs.py target/doc
```

Keep the first two Cargo commands separate. They supply the metadata for links to
inherited and re-exported APIs before rustdoc builds the workspace.
Links to guides and examples use the source commit. They cannot show uncommitted edits.

The last command checks the list of crate pages, prepares the site root, and copies both licences.
Each source page names the published file, not its location on the build machine.
The command stops publication if it finds a build path in the output.

## Configure automatic publication

1. Protect the default branch. Require reviewed workflow changes and signed commits.
2. Configure the `documentation` environment to accept only that branch. Do not allow tag deployments or administrator bypass.
3. Register the dedicated public signing key. Check its fingerprint against the publisher.
4. Set `DOCS_SIGNING_PRIVATE_KEY`, `DOCS_SIGNING_PASSPHRASE`, and `INDEX_REPO_TOKEN` in that environment.
   Give the fine-grained token contents read and write access to the documentation index only.
5. Before the credential check, find all three names under **Environment secrets** for `documentation`.
   Remove any repository-level copies. Check that no organisation secret with those names is available to this repository.
   This prevents a repository or organisation secret from masking a missing environment secret.
6. Keep required reviewers for the credential check. Open **Publish Documentation** in GitHub Actions.
   Select the default branch as the workflow source. For **Ref to check**, select **branch**.
   For **Exact branch or tag name**, enter the default branch name.
   Select **Check credentials and sign a local commit without publishing**. This option is off by default.
7. Start the workflow. Approve the credential check.
   Check that the run tested write access without pushing and signed a local commit.
8. Only after the check passes, remove the required reviewers for unattended publication.
   If you keep them, documentation updates still need approval.

The signing job runs on a fresh runner. It uses only trusted publication code from the default branch and the pinned site publisher.
The private key enters only the import step. The GPG wrapper passes the passphrase through a separate file descriptor.
The workflow checks the fingerprint before signing. It stops the GPG agent and removes the temporary keyring after publication.

The dedicated key supports separate revocation. It can sign commits outside RCA under its registered GitHub account.
Keep recovery copies outside the checkout. If the key is compromised, revoke it and replace it before publication resumes.

## Repair one ref

After a failed build or site deployment, open **Publish Documentation** in GitHub Actions.
Select the default branch as the workflow source. Enter the exact ref name and select its kind: **branch** or **tag**.
Before you run the repair, check both fields. If that ref is absent from Git, the repair requests **removal** of its published snapshot.
If the ref exists, the repair builds its current commit. It does not accept an archive or commit from the caller.

After site deployment, check the result for that ref:

- **Update:** Open `docs-meta.json` at the root of its published snapshot. Compare `commitSha` with the intended source commit.
- **Removal:** In the [RCA inventory](https://docs.jacobcx.dev/repo/1123147933/publications.json), check that no entry has that `refType` and `refName`.
  If another published ref has the same name but a different kind, check that its entry remains.

A successful local build or credential check does not prove that the site deployed.
