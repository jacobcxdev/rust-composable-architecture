#!/usr/bin/env bash
set -euo pipefail
readonly repo=/Users/jacob/.codex/worktrees/rca-runtime-foundation
readonly evidence="$repo/.superpowers/sdd/2026-08-24-rca-composition-macros-and-qualification/task-5-model-qualification/binaries-a20ca75"
readonly toolchain=/Users/jacob/.rustup/toolchains/1.98.0-aarch64-apple-darwin
export DYLD_LIBRARY_PATH="$toolchain/lib"
objcopy_bin="$("$toolchain/bin/rustc" --print target-libdir)/../bin/rust-objcopy"
readonly destination="$repo/target/binaries-a20ca75/corrected-stripped"
test ! -e "$destination"
mkdir "$destination"
"$objcopy_bin" --version
printf 'SOURCE_REVISION=a20ca75971ba7746d847c9a598c2831cd9f62289\n'
printf 'SOURCE=preserved clean-build outputs; input hashes checked against original receipt\n'
record() {
    local label="$1" source="$2" expected_hash
    expected_hash=$(awk -v label="$label" '$0 == "binary=" label {found=1; next} found && /^sha256=/ {sub(/^sha256=/, ""); print; exit}' "$evidence/binary-sizes.txt")
    test -n "$expected_hash"
    test "$(shasum -a 256 "$source" | awk '{print $1}')" = "$expected_hash"
    cp "$source" "$destination/$label"
    "$objcopy_bin" --strip-all "$destination/$label"
    printf 'binary=%s\ninput_sha256=%s\nbytes=%s\nsha256=%s\n' "$label" "$expected_hash" "$(stat -f '%z' "$destination/$label")" "$(shasum -a 256 "$destination/$label" | awk '{print $1}')"
}
for fixture in direct composed-manual composed-macro scoped; do
    record "$fixture" "$repo/target/binaries-a20ca75/fixtures/release/rca-compile-$fixture"
done
test_executable=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "app") | .executable // empty' "$evidence/app-test-build.jsonl" | tail -1)
record deterministic-composition-test "$test_executable"
record tokio-application "$repo/target/binaries-a20ca75/root/release/rca-example-composition-search"
printf 'EXIT=0\n'
