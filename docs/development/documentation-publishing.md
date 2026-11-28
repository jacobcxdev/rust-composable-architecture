# Publish the API documentation

The publication workflow builds a separate API reference for each RCA branch and tag on the [documentation site](https://docs.jacobcx.dev/).
If the site has no build for your revision, [build a local preview](#build-a-local-preview).
Readers can choose a branch to see work in progress or a tag to see a fixed revision.
The site highlights `main` as stable and `dev/next` as development. Other branches also get previews.
A branch and a tag with the same name keep separate pages.

- To review a change without publishing anything, [build a local preview](#build-a-local-preview).
- To find out what GitHub built, [check a build](#check-a-build).
- To correct missing or stale pages, [repair one ref](#repair-one-ref).
- To enrol the repository or replace a credential, [configure automatic publication](#configure-automatic-publication).

Publishing these pages does not upload any Rust package. For crate publication, see [release RCA](releasing.md).

## What happens after a push

A Git *ref* is a named reference, such as a branch or tag, to a revision.
The [push workflow](../../.github/workflows/docs-push.yml) notifies the [publisher](../../.github/workflows/publish-docs.yml) on the default branch.
The publisher checks the current ref before it builds. If the ref no longer names the pushed commit, the publisher ignores the notification.
Pull requests from forks do not publish pages to this site.

The build uses no signing key, site token, or shared compiler cache.
A separate job receives the archive from the same run. It checks the archive's digest, a hash of its contents, before it signs the site update.
For a deletion, the publisher checks that the ref still does not exist. A different ref of the same name keeps its own pages.

Git and the site can change at different times. For example, a branch can move again after the publisher's last check.
A failed build or deployment can leave old pages online or no pages at all.
Old branches can lack the push workflow. GitHub can omit events for bulk tag changes.
Each day, a scheduled job compares live refs with the [site inventory](https://docs.jacobcx.dev/repo/1123147933/publications.json) and requests repairs.

## Check a build

1. In GitHub Actions, open the **Publish Documentation** run. **Documentation Push** only sends a notification.
2. In **Checkout source at the verified commit**, check the source commit. The workflow itself runs from the default branch, which can point to another commit.
3. Within seven days, download the `rca-api-reference-*` archive from that run.
4. Extract the archive.
5. Open `index.html` to inspect the generated reference.

A deletion or an ignored notification produces no build archive. A successful build does not prove that the website deployed it.

## Build a local preview

The [preview tool](../../scripts/build_doc_preview.py) builds the public libraries with all features. It treats documentation warnings as errors and excludes examples and test programs.
It uses a fixed Rust container image. The list of packages comes from the source revision, not the current release.

**Any supported source revision, on Linux:** Use an isolated runner with Docker, Python 3, and Git.
Start in the checkout that contains the trusted preview tool.
Set `source_checkout` to a clean checkout of the revision you want to read.
Set `output` to a new path outside that checkout, under an existing directory.
Run:

```sh
python3 scripts/build_doc_preview.py "$source_checkout" "$output" --revision "$(git -C "$source_checkout" rev-parse HEAD)"
```

After it succeeds, open `index.html` in the output directory.

**Current checkout only:** Use the [repository toolchain](../../CONTRIBUTING.md#toolchain-and-formatting) from the root of a trusted checkout.
In a POSIX shell, create a fresh Cargo target directory. This keeps outdated crate pages out of the preview.

```sh
export CARGO_TARGET_DIR="$(mktemp -d)"
export RCA_DOCS_REVISION="$(git rev-parse HEAD)"
export RUSTDOCFLAGS='-D warnings --cfg rca_docs_source'
```

On macOS, define this command to run rustdoc through the [memory watchdog](../../CONTRIBUTING.md#build-memory-safety):

```sh
build_docs() {
  python3 scripts/guard_cargo_memory.py --memory-limit-gib 3 --receipt "$(mktemp -d "$CARGO_TARGET_DIR/guard.XXXXXX")/receipt.json" -- doc "$@"
}
```

This command limits the build to three GiB in addition to the guard's four-GiB host reserve.
If the guard refuses the build, leave it enabled. Use an isolated runner with an operating-system memory limit instead:

```sh
build_docs() { rustup run 1.100.0 cargo doc "$@"; }
```

In the same shell, run:

```sh
build_docs --locked -p futures-core --no-deps
build_docs --locked -p rca-event-listener --all-features --no-deps
build_docs --locked --workspace --lib --exclude 'rca-example-*' --exclude rca-size-probe --all-features --no-deps
python3 scripts/prepare_public_docs.py "$CARGO_TARGET_DIR/doc"
```

Open `$CARGO_TARGET_DIR/doc/index.html`. Links to guides and examples use the commit in `RCA_DOCS_REVISION`, not uncommitted edits.
Keep the first two Cargo commands separate. They supply link information for inherited and re-exported APIs.
The last command checks the crate pages, creates the landing page, and copies both licences.
It replaces local source paths in page descriptions and rejects output that still contains a known local build path.
After the preview, discard only the temporary target directory you created. Unset the three exported variables before your next build.

## Configure automatic publication

Only a repository administrator needs this procedure. A GitHub *environment* restricts which jobs can use its credentials.
The publication environment is `documentation`.

1. Protect the default branch. Require reviewed workflow changes and signed commits.
2. Restrict the environment to jobs from the default branch. Disable tag deployments and administrator bypass.
3. Register the dedicated public signing key on GitHub. Match its fingerprint to `DOCS_SIGNING_FINGERPRINT` in the publisher workflow.
4. Store `DOCS_SIGNING_PRIVATE_KEY`, `DOCS_SIGNING_PASSPHRASE`, and `INDEX_REPO_TOKEN` under **Environment secrets**.
   Restrict the fine-grained token to contents read and write access on the documentation index repository.
5. Before the credential check, check that the environment lists all three secret names.
   Remove repository-level copies. Make sure that the repository cannot access organisation secrets with those names.
   Otherwise, a different secret can mask a missing environment secret.
6. Keep required reviewers in place for the credential check.
   Open **Publish Documentation** in Actions. Select the default branch as the workflow source.
   For **Ref to check**, select **branch**. For **Exact branch or tag name**, enter the default branch name.
7. Select **Check credentials and sign a local commit without publishing**. This option is off by default.
   Start the workflow. Approve the credential check.
8. Check that the job tested write access without pushing and signed a local commit.
   Only after a successful check, remove the required reviewers to enable unattended publication.

The signing job uses trusted code from the default branch and a fixed revision of the site publisher.
Only the key import step receives the private key. The signing program supplies the passphrase separately from its command arguments.
The job checks the key fingerprint, then signs. It stops the signing agent and removes the temporary keyring after use.

Keep recovery copies outside the checkout. This key can also sign commits outside RCA under its registered GitHub account.
If the key leaks, revoke it. Replace it before publication resumes.

## Repair one ref

1. In GitHub Actions, open **Publish Documentation**. Select the default branch as the workflow source.
2. Select the ref kind, **branch** or **tag**. Enter its exact name.
3. **Before you start, check the name and kind.** If that ref no longer exists, the repair requests removal of its pages.
4. Run the workflow. For an existing ref, it builds the current commit. It does not accept a caller's archive or commit.
5. After site deployment, check the result:
   - **Update:** Open `docs-meta.json` at the root of that ref's published pages. Compare `commitSha` with the intended commit.
   - **Removal:** In the [site inventory](https://docs.jacobcx.dev/repo/1123147933/publications.json), check that no entry has both that `refType` and that `refName`.
     If another kind of ref has the same name, its entry must remain.
