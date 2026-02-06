	alias ~~~=":<<'~~~sh'";:<<'~~~sh'

# Publishing

Steps required to create (and publish) a staging copy of the documentation that will eventually be
published to the [docs.rs]() page for this crate.[^sh]

[^sh]: Because this README file [is also a valid Bourne shell script](https://gist.github.com/bwoods/1c25cb7723a06a076c2152a2781d4d49),
sourcing it will do these steps automatically.

## Ensure semantic versioning

As this crate has not been published yet, for now, we must explicitly set the baseline to compare the
public interface to.

~~~sh
cargo semver-checks --package composable --baseline-rev v0.7.0 || exit
~~~

If there are any [detected violations](https://github.com/obi1kenobi/cargo-semver-checks) of
semver, the crate should not be published until the version number has been updated accordingly. Or
if the change was accidental, the breakage should be fixed.

> [!TIP]
>
> - If the version number does need to be updated, remember to change it in the
>   [top-level README](../README.md) as well.
> - `cargo semver-checks` itself can be updated with `cargo install-update -a`
>   (assuming `cargo install cargo-update` was installed).

## Generating the documentation

These steps should be done within the `about` directory.

~~~sh
cd "$(git rev-parse --show-toplevel)/about" || exit
~~~

A fresh build of all of the crate documentation is performed, ensuring that no "out of date" files
are left in place.

~~~sh
rm -rf ../target/doc/ || true
~~~

The nightly version of **rustdoc** is used so that the unstable `feature(doc_auto_cfg)` can be used
to [indicate feature-gated items in documentation](https://github.com/rust-random/rand/issues/986).
Look for the `docsrs` flag in the crate source to see how it is used.

~~~sh
RUSTDOCFLAGS="--cfg docsrs" \
cargo +nightly doc --package composable --package composable-views \
                   --no-deps --all-features || exit
~~~

GitHub Pages requires an `index.html` page at the root of the documentation branch, whereas
**rustdoc** nests it within a folder named after the crate.
[As discussed here](https://dev.to/deciduously/prepare-your-rust-api-docs-for-github-pages-2n5i), a
simple redirect can be put in place.

~~~sh
echo "<meta http-equiv='refresh' content='0; url=composable'>" \
   > ../target/doc/index.html
~~~

### Pushing to the documentation branch

Now that the documentation has been generated, it must be pushed to the
[appropriate branch](https://github.com/jacobcxdev/rust-composable-architecture/tree/docs.rs) on
GitHub.

~~~sh
cd ../target/doc/
rm -rf .git/ || true
git init --quiet --initial-branch=docs.rs
rm .lock # remove the lock file; we won't need it
~~~

Although git is being used to manage the documentation files, there is no need to preserve the
history of this branch. It is recreated every time.

~~~sh
git add --all
git commit --quiet --allow-empty-message -m ""
~~~

After all of the files have been added, they are pushed to the remote branch.

~~~sh
git remote add -m docs.rs github https://github.com/jacobcxdev/rust-composable-architecture.git
#git push --force --set-upstream github docs.rs
~~~

Since this branch shares no history with any previous version pushed to the repository, a `--force`
push is required.

### Viewing the documentation

The crate documentation should be visible at the
[GitHub Pages URL for the repository (stable)](https://docs.jacobcx.dev/jacobcxdev/rust-composable-architecture/stable/index.html).
