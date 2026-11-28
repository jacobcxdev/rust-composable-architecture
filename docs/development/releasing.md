# Publish RCA to crates.io

Use this procedure to publish a reviewed RCA release to the Rust package registry.
It requires access to the release signing key, the repository, and the `crates-io` environment in GitHub Actions.
That environment controls who can approve a crate upload. **Do not push a release tag as a test.** A new signed release tag starts the [publication workflow](../../.github/workflows/publish-crates.yml).
Merging a pull request does not publish packages.

RCA releases ten crates together. The optional coding-agent plugin shares their version but ships from the Git repository, not crates.io.
The main package is `rca`. The long-name package, `rust-composable-architecture`, exposes the same API and features.
Cargo publishes `rca` before the package that depends on it.

## Before you tag

1. Review every change on `origin/main` since the last release. For the first full release, review the whole project.
2. Complete the [documentation and skill review](#review-documentation-and-skills).
   Check the package versions, required continuous integration (CI) checks, and [release qualification](qualification.md).
3. Check the existing Git tags, [GitHub releases](https://github.com/jacobcxdev/rust-composable-architecture/releases), and crates.io versions.
   Use a maintainer account to see draft releases. Resolve any partial or conflicting release first.
4. Select the next version according to the [release sequence](../../scripts/check_release_tag.py).
   Do not skip a patch, minor, or major number within the chosen release type.
5. For the first full release, [configure the upload token](#first-release-set-up-the-credential) before creating the tag.
   For later releases, check [trusted publishing](#later-releases-use-trusted-publishing) for each crate.
6. Fetch `origin/main` immediately before tagging. Make sure that its tip is the reviewed, merged commit.
7. Create a signed, **annotated** tag directly on that commit. An annotated tag carries the release name, a message, and the signature.
   Push only that tag. Even if the files match, do not tag a pull request's head.
8. Until the upload finishes, keep `main` at that commit.

### Review documentation and skills

Before you sign the tag, compare the README, guides, API reference, examples, and [agent skills](../../plugins/rca/skills) with the proposed release.
If a reader cannot use the documented API as shown, fix it through a pull request.

- For each changed public behaviour, compare the instructions with a current example or test. Run the affected example or documentation test.
- For each deprecated API, record the version, replacement, and migration steps in the [changelog](../../CHANGELOG.md).
  Link to the replacement from the old API. Update affected examples and skills.
  If no replacement exists, tell callers what they can do. Record removals in the release that removes the API.
- Check the installation instructions. If a skill or plugin changed, install it from the candidate checkout.
  Try an affected task. Compare the result with the real API or contributor workflow.
- Keep general guidance free of release numbers, commit hashes, and private machine details. State compatibility limits where they apply.
  Match the version in the [plugin manifest](../../plugins/rca/plugin.json) to the workspace version.

The signed tag identifies one revision of the code, guides, examples, and skills. Do not move an old tag to change its guidance.

## First release: set up the credential

The `v0.0.1` preview covered only `rca` and `rust-composable-architecture`. Check that crates.io lists both preview packages.
A Git tag alone does not prove that an upload succeeded.

1. Complete the crates.io email verification process.
2. Create a short-lived token with `publish-new` for the new crates. For the two preview crates, grant `publish-update`.
   Restrict the token to these names:

   | Purpose | Packages |
   | --- | --- |
   | Application API, implementation, and macros | `rca`, `rust-composable-architecture`, `rca-core`, `rca-macros` |
   | Tests, runtime, and storage | `rca-test`, `rca-tokio`, `rca-persistence` |
   | Channels and listeners | `rca-async-channel`, `rca-event-listener`, `rca-event-listener-strategy` |

3. Enter the token directly in GitHub as the `CRATES_IO_BOOTSTRAP_TOKEN` secret of the `crates-io` environment.
   Do not store it as a repository secret. Never put it in a commit, issue, chat, or workflow log.

Allow time for the initial upload. The registry can limit how often an account creates a new crate.

## What you approve

The first job checks that the signed tag points to the current tip of `main`. It checks the crate and plugin versions against the tag.
It builds and tests all ten crate archives without upload credentials.
Its summary links to the source changes and review checklist at that commit.

Before approving the **crates-io** deployment, review the summary, the workflow, and the archive results.
Your approval records the documentation and skill review and permits the upload. If that review is incomplete, do not approve.

After approval, the next job packages the crates again.
It compares the archive hashes with the first job's results. A matching hash identifies identical archive bytes.
For versions the registry already lists with the same checksum, it skips the upload.
For missing versions, Cargo uploads the archives in dependency order.
Before each upload, the credential helper checks the checksum Cargo requests. A mismatch denies Cargo the upload token.
The final check requires the public registry to list all ten expected checksums.

**If an upload stops:** Inspect the failed step and existing registry entries.
Fix the cause first. If the tag's commit is still the tip of `main`, rerun the failed job on the **same tag**.
The workflow cannot replace an existing version. If the tag, commit, or published bytes are wrong, stop.
Do not move the tag or retry with different files.

## Record the performance baseline

The first full runtime release supplies the reference for later measurements. The preview contains no runtime to measure.
The tag identifies the source. The registry identifies the bytes that application authors can install. A GitHub Release page alone establishes neither fact.

After all uploads finish, fetch the release tag. From the repository root, run:

```sh
python3 scripts/release_baseline.py record
```

The command prints the baseline record. Compare its signed tag, commit, source tree, and dependency-lock hash with the qualification record.
If the registry has only a partial release or the records differ, stop. Do not substitute a development commit.

Keep a private copy of the record alongside the runner profile, ordered measurements, and guard receipts.
Include the profile and measurement file hashes, toolchain, workloads, and benchmark rules.
Another maintainer can then repeat the checks after the CI artefacts expire.
Future qualification compares the candidate with the [last full published release](qualification.md#native-review-and-release).

## Later releases: use trusted publishing

After the first full release, configure [crates.io trusted publishing](https://crates.io/docs/trusted-publishing) for **each** crate.
Trusted publishing gives an approved GitHub job a temporary upload token. You no longer need a stored registry token.

1. On each crate, specify repository `jacobcxdev/rust-composable-architecture`, workflow filename `publish-crates.yml`, and environment `crates-io`.
2. Keep the pinned revision of the [official sign-in action](https://github.com/rust-lang/crates-io-auth-action) in the repository's GitHub Actions allowlist.
3. Revoke the first-release token. Remove its secret from the environment.

The `crates-io` environment still requires approval for crate uploads.
[Documentation publication](documentation-publishing.md#configure-automatic-publication) uses separate credentials and can run without approval after configuration.
Check the hosted pages separately from the published crates.
