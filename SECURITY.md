# Security policy

## Report a vulnerability privately

Email the maintainer at [me@jacobcx.dev](mailto:me@jacobcx.dev).
Use the subject `RCA security report`.
Do not open a public issue or pull request with vulnerability details.
Do not include credentials, private keys or personal data in the report.

Include:

- The affected package and version or Git revision.
- The target platform, Rust version and enabled features.
- The expected result, actual result and security impact.
- A minimal reproduction with artificial data.
- Any known workaround and a safe way to contact you.

If the reproduction contains sensitive material, ask for a suitable transfer method first.
Ordinary email is not an end-to-end encrypted channel.

The maintainer will assess the report and discuss remediation and disclosure with you.
This project does not promise a response deadline, bounty or backport to every version.

## Supported versions

RCA 0.1.0 is an unpublished candidate, not a stable release. Local
release-readiness qualification is complete for the recorded candidate; hosted
PR CI has not yet run. Reports about the candidate are welcome. The
[support policy](docs/support.md) defines intended platforms, features,
compatibility and the qualification boundary.
Historical Composable releases and retired prototypes are not the current RCA candidate.

Release qualification includes dependency advisory checks. A passing check does not
establish that RCA or its dependencies have no vulnerabilities.

## Coordinating a repair

Keep sensitive details private during assessment. Agree on disclosure timing with
the maintainer before a public report. Include a regression check in the repair.
Changes to safety, runtime or dependency contracts require their normal release gates.
Security urgency does not make an untested fix qualified.
