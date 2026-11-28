# Security policy

## Report a vulnerability privately

Email the maintainer at [me@jacobcx.dev](mailto:me@jacobcx.dev).
Use the subject `RCA security report`.
You can also use [GitHub's private reporting form](https://github.com/jacobcxdev/rust-composable-architecture/security/advisories/new).
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

The maintainer will assess the report and discuss a fix and disclosure timing with you.
This project does not promise a response deadline, bounty or backport to every version.

## Supported versions

Security reports are welcome for the current release series and the development branch.
Include the affected version or commit so that the maintainer can reproduce the problem.
The [support policy](docs/support.md) defines platforms, features and compatibility rules.
This policy covers RCA, not other projects or retired prototypes.

Release checks include a search for known dependency advisories. A passing check
does not rule out vulnerabilities in RCA or its dependencies.

## Coordinating a repair

Keep sensitive details private during assessment. Before publishing a report,
agree on disclosure timing with the maintainer. Include a regression check in
the repair. Even for an urgent fix, run the usual release checks for changes
to safety, runtime or dependency contracts.
