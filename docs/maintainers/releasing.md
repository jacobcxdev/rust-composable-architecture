# Publish RCA to crates.io

RCA ships ten crates and an optional coding-agent plugin at the same version.
Its main package is `rca`. The `rust-composable-architecture` package re-exports
`rca` at the exact same version and forwards its public features. Cargo publishes
`rca` before its re-export.
Merging a pull request does not publish a crate. A new, signed release tag
starts the [publication workflow](../../.github/workflows/publish-crates.yml).
Only the maintainer can approve the upload.

The `v0.0.1` preview tag covers only `rca` and `rust-composable-architecture`.
Before the first full release, check that crates.io lists both preview packages.
A Git tag alone does not publish a package.

## Before you tag

1. Review the changes on `origin/main` since the last release. For the first
   full release, review the whole project. Complete the [documentation and skill review](#review-documentation-and-skills).
   Check the release versions, required CI checks and [release qualification](qualification.md).
2. Check the repository's tags, [GitHub releases](https://github.com/jacobcxdev/rust-composable-architecture/releases)
   and crates.io. Use a maintainer account to see draft releases. Resolve any
   partial or conflicting release first. Follow the [release sequence](../../scripts/check_release_tag.py).
   Do not skip a patch, minor or major number within the chosen release type.
3. Fetch `origin/main` immediately before tagging. Sign an **annotated** tag
   on that exact commit and push only that tag. Do not tag the pull request's
   head, even if its files match. Keep `main` at the reviewed commit until the
   upload finishes.

### Review documentation and skills

Do this review before you sign the tag. If the review finds a gap, fix it through a pull request.

- Compare the README, guides, API docs, examples and [agent skills](../../plugins/rca/skills) with the changes since the last release.
- For each changed public behavior, check its instructions against a current example or test. Run the affected example or doctest.
- For each deprecation, record the version, replacement and migration steps in the [changelog](../../CHANGELOG.md).
  Point readers to the replacement from the old API. Update affected examples and skills to use it.
  If no replacement exists, state what callers can do. Record removals in the release that removes the API.
- Check the installation instructions. If the plugin or a skill changed, install from the candidate checkout.
  Try an affected task with that installation. Check the result against the real API or repository workflow.
- Keep general guidance free of release numbers, commit hashes and private machine details.
  Keep compatibility limits explicit. Use the workspace version in the [plugin manifest](../../plugins/rca/plugin.json).

The signed tag identifies one revision of the code, guides, examples and skills.
Later fixes go into a new revision. Do not edit or move an old tag to change its guidance.

## First release: set up the credential

Before the first full release tag, complete email verification on crates.io.
Create a short-lived token with `publish-new` for the new crates.
For the two preview crates, grant `publish-update`. Limit the token to these names:

| Use | Package names |
| --- | --- |
| Application API and implementation | `rca`, `rust-composable-architecture`, `rca-core`, `rca-macros` |
| Testing, runtime and storage | `rca-test`, `rca-tokio`, `rca-persistence` |
| Channels and listeners | `rca-async-channel`, `rca-event-listener`, `rca-event-listener-strategy` |

Enter the token directly in GitHub as `CRATES_IO_BOOTSTRAP_TOKEN` in the
`crates-io` **environment**, not as a repository secret. Allow enough time
for the first upload: crates.io can limit how often an account creates a new
crate. Never put the token in a commit, issue, chat or workflow log.

## What you approve

The first job checks the signed tag against current `main` and the crate and plugin versions against the tag.
It builds the ten crate archives without upload credentials.
Its summary links to the changes and the checklist at the tagged commit.
Before you approve the `crates-io` deployment, check that summary, its workflow and the archive results.
Your approval records your documentation and skill review and permits the crate upload.
CI cannot judge whether advice or migration steps are correct. If the review is incomplete, do not approve.

After approval, the job packages the crates again. It checks that the hashes
match the archives tested by the first job. It then checks the public crates.io
index and skips versions whose checksums already match.

Cargo uploads the remaining crates in dependency order. Before each upload,
Cargo must present the checksum of the file it will send. If the checksum
differs, Cargo gets no upload token. Publication succeeds only when the public
index lists the expected checksum for **all ten** archives. A green PR check
or a successful dry run does not publish a crate.

If an upload stops partway through, inspect the failed step and registry
entries. Once the cause is resolved, rerun the failed job on the **same
tag**, while its commit is still the tip of `main`. The workflow will not
replace an existing version or accept a checksum mismatch. If the tag,
commit or published content is wrong, stop. Do not move the tag or retry with
different files.

## Record the performance baseline

The first full runtime release establishes the reference for later releases. The preview has no runtime to measure.
A tag identifies the source. The registry records what users can install. A GitHub Release page alone does not establish either fact.

After all crate uploads finish, fetch the release tag and run:

```sh
python3 scripts/release_baseline.py record
```

Check the signed tag, resolved commit, source tree, and dependency-lock hash against the qualification record.
Keep the record with the release evidence outside Git. Include the toolchain, workload and protocol identities, and hashes of the measurement files.
If the registry has only a partial release or the identities differ, stop. Do not substitute a development commit.
Future qualification compares the candidate with the [last full published release](qualification.md#native-review-and-release).

## Later releases: use trusted publishing

After the first release, set up [crates.io trusted publishing](https://crates.io/docs/trusted-publishing)
on **each** crate. Use repository `jacobcxdev/rust-composable-architecture`,
workflow filename `publish-crates.yml` and environment `crates-io`. Revoke
the first-release token and remove its environment secret.

Later releases use the [official crates.io sign-in action](https://github.com/rust-lang/crates-io-auth-action)
to obtain a temporary token after approval. Keep the workflow's pinned
action revision in the repository's Actions allowlist.
Crate uploads require approval in the `crates-io` environment.
[Documentation publication](documentation-publishing.md#configure-automatic-publication) uses a separate workflow and environment.
The maintainer approves the initial credential check. After configuration, documentation updates need approval only if that environment retains required reviewers.
Check the hosted pages separately.
