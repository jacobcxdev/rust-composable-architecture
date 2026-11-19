#!/usr/bin/env bash
set -euo pipefail
readonly repo=/Users/jacob/.codex/worktrees/rca-composition-reference-20260923
readonly expected=b9eb6e732d2b0cf6ce3cd7255a5958781587af6c
readonly toolchain=/Users/jacob/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin
readonly evidence=/Users/jacob/.codex/worktrees/rca-runtime-foundation/.superpowers/sdd/2026-08-24-rca-composition-macros-and-qualification/task-5-model-qualification/binaries-reference-corrected
readonly target_root=/Users/jacob/.codex/worktrees/rca-runtime-foundation/target/binaries-reference-corrected
cd "$repo"
guard() {
    test "$(git rev-parse HEAD)" = "$expected"
    test -z "$(git status --porcelain)"
}
guard
if env | grep -Eq '^CARGO_PROFILE_'; then
    echo 'unexpected ambient profile override' >&2
    exit 1
fi
export PATH="$toolchain:$PATH"
export RUSTC="$toolchain/rustc" RUSTDOC="$toolchain/rustdoc"
export RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= CARGO_BUILD_JOBS=1
export DYLD_LIBRARY_PATH="${toolchain%/bin}/lib"
objcopy_bin="$("$RUSTC" --print target-libdir)/../bin/rust-objcopy"
printf 'HEAD=%s\nSTATUS=clean\nSTART_UTC=%s\nDYLD_LIBRARY_PATH=%s\nOBJCOPY=%s\n' "$expected" "$(date -u +%FT%TZ)" "$DYLD_LIBRARY_PATH" "$objcopy_bin"
"$toolchain/cargo" --version
"$RUSTC" --version
"$objcopy_bin" --version
sw_vers
shasum -a 256 Cargo.toml Cargo.lock benches/compile/Cargo.toml benches/compile/Cargo.lock benches/compile/{direct,composed-manual,composed-macro,scoped}/src/main.rs examples/composition-search/src/main.rs examples/composition-search/tests/app.rs
set -x
env CARGO_TARGET_DIR="$target_root/fixtures" "$toolchain/cargo" build --manifest-path benches/compile/Cargo.toml --workspace --release --locked -j 1
env CARGO_TARGET_DIR="$target_root/root" "$toolchain/cargo" build -p rca-example-composition-search --release --locked -j 1
env CARGO_TARGET_DIR="$target_root/root" "$toolchain/cargo" test -p rca-example-composition-search --test app --release --locked -j 1 --no-run --message-format=json > "$evidence/app-test-build.jsonl"
set +x
test_executable=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "app") | .executable // empty' "$evidence/app-test-build.jsonl" | tail -1)
test ! -e "$evidence/binary-sizes.txt"
record() {
    test -x "$2"
    printf 'STRIP=%s\n' "$1"
    "$objcopy_bin" --strip-all "$2"
    {
        printf 'binary=%s\nbytes=%s\n' "$1" "$(stat -f '%z' "$2")"
        printf 'sha256=%s\n' "$(shasum -a 256 "$2" | awk '{print $1}')"
        file -b "$2" | sed 's/^/identity=/'
    } >> "$evidence/binary-sizes.txt"
}
for fixture in direct composed-manual composed-macro scoped; do
    record "$fixture" "$target_root/fixtures/release/rca-compile-$fixture"
done
record deterministic-composition-test "$test_executable"
record tokio-application "$target_root/root/release/rca-example-composition-search"
guard
printf 'EXIT=0\nEND_HEAD=%s\nEND_STATUS=clean\nEND_UTC=%s\n' "$expected" "$(date -u +%FT%TZ)"
