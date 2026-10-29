#!/usr/bin/env bash
set -euo pipefail

baseline_dir="docs/performance/baselines"
rust98_cargo="$(rustup which --toolchain 1.98.0 cargo)"
rust98_rustc="$(rustup which --toolchain 1.98.0 rustc)"

export RUSTC="${rust98_rustc}"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "baseline capture requires a clean tracked worktree" >&2
  exit 1
fi

if ! git diff --quiet v0.7.0 -- \
  Cargo.toml Cargo.lock composable-architecture composable-views about LICENSE README.md; then
  echo "tracked runtime sources differ from v0.7.0" >&2
  exit 1
fi

mkdir -p "${baseline_dir}"

{
  printf 'source-revision: %s\n' "$(git rev-parse v0.7.0)"
  printf 'source-equivalence: tracked runtime sources match v0.7.0\n'
  printf 'rustc-path: %s\n' "${rust98_rustc}"
  printf 'cargo-path: %s\n' "${rust98_cargo}"
  "${rust98_rustc}" --version --verbose
  "${rust98_cargo}" --version --verbose
  uname -a
  sysctl -n machdep.cpu.brand_string 2>/dev/null || true
  sysctl -n hw.memsize 2>/dev/null || true
} > "${baseline_dir}/0.7-system.txt"

"${rust98_cargo}" clean
{
  /usr/bin/time -lp "${rust98_cargo}" build --workspace --release
  find target/release -maxdepth 1 -type f -perm -111 -exec stat -f '%N %z' {} \;
} > "${baseline_dir}/0.7-build.txt" 2>&1

"${rust98_cargo}" bench --workspace --no-fail-fast \
  > "${baseline_dir}/0.7-divan.txt" 2>&1
