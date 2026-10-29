# Keyed lifecycle fuzz qualification

This independent package is excluded from normal workspace builds. The keyed
lifecycle target decodes four-byte operation chunks and accepts at most 512
chunks, so its model bound corresponds exactly to `-max_len=2048`.

## Reproduce the fixed qualification

Run from the repository root. The first corpus directory below is writable;
the second is the committed seed corpus and remains unchanged.

```sh
repo_root=$(git rev-parse --show-toplevel)
stable_bin=$(dirname "$(rustup which --toolchain 1.98.0 cargo)")
nightly_bin=$(dirname "$(rustup which --toolchain nightly-2026-08-23 cargo)")
tool_root=$(mktemp -d /tmp/rca-cargo-fuzz-0.13.2.XXXXXX)
corpus_dir=$(mktemp -d /tmp/rca-keyed-corpus.XXXXXX)
artifact_dir=$(mktemp -d /tmp/rca-keyed-artifacts.XXXXXX)
log_file=$(mktemp /tmp/rca-keyed-fuzz.XXXXXX)

PATH="$stable_bin:$PATH" CARGO_BUILD_JOBS=1 \
  cargo install cargo-fuzz --version 0.13.2 --locked --root "$tool_root"
PATH="$tool_root/bin:$nightly_bin:$PATH" \
  RUSTUP_TOOLCHAIN=nightly-2026-08-23 \
  RUSTC="$nightly_bin/rustc" RUSTDOC="$nightly_bin/rustdoc" \
  RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= CARGO_BUILD_JOBS=1 \
  cargo fuzz run keyed_lifecycle \
    --fuzz-dir "$repo_root/fuzz" \
    "$corpus_dir" \
    "$repo_root/fuzz/seeds/keyed_lifecycle" \
    -- -runs=1000000 -seed=824202604 -max_len=2048 \
       -artifact_prefix="$artifact_dir/" \
    > "$log_file" 2>&1
```

The compiler paths and toolchain override prevent a repository default or
inherited `RUSTC` from selecting another compiler. If local Cargo configuration
sets C/C++ compiler wrappers, ensure those wrappers are available or override
`HOST_CC`, `HOST_CXX`, `CC` and `CXX` for this invocation with installed compilers.
On the macOS qualification host these were `/usr/bin/clang` and
`/usr/bin/clang++`; no global Cargo configuration was changed.

The committed seed `seeds/keyed_lifecycle/basic` has SHA-256
`f8cb3ef435ed15d5f63f07884a3dd7587ff0c7d19b3ea3b944ed3db978df3eb4`.

## Latest qualification

On 28 October 2026, the current million-run campaign completed with exit 0 and
no crash artefacts. The 129 captured compiled source identities still matched
after completion. The exact source, compiler, command, counters and scope limits
remain in the retained local audit receipt. See the
[public qualification boundary](../docs/qualification.md) for the evidence limits.

## Historical qualification

On 30 September 2026, an earlier million-run campaign completed with exit 0 and no
crash artefacts. It started at `2e8d40787b43f23a066874674ddd3a906432f3b9`;
product and fuzz-target sources remained unchanged while independent test and
documentation commits landed. That development revision and its full receipt
remain in local history. They are not reproduction inputs supplied by the public snapshot.

An earlier 100,000-run invocation passed with `cargo-fuzz 0.13.2` and
`nightly-2026-08-23`: exit 0 after 100,000 runs, `cov: 3531`, `ft: 16417`,
`corp: 1106/40Kb`, and zero crash artifacts. The complete log had SHA-256
`e33d908d47530c52d9ccacfb3aba325b41ed7ead288a1018438b8860f3ddcd66`.

Evolved corpus entries and run logs are generated qualification evidence. They
are intentionally kept outside the repository and are not committed.
