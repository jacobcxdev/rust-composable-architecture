#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <new-raw-output-directory>" >&2
  exit 2
fi

for command in awk cargo cat date dirname env file git grep jq mktemp python3 rm rustup sed shasum stat sw_vers sysctl tail touch tr uname; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command not found: ${command}" >&2
    exit 1
  fi
done

runner_source_sha256=$(shasum -a 256 "$0" | sed 's/[[:space:]].*//')
script_dir=$(cd "$(dirname "$0")" && pwd)
repo_root=$(git rev-parse --show-toplevel)
cd "${repo_root}"
allocation_manifest=${script_dir}/composition-allocation-workloads.tsv
evaluator=${script_dir}/composition_performance.py
if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  echo "composition qualification requires a clean checkout" >&2
  exit 1
fi

if [[ -n "${RUSTC_WRAPPER:-}" || -n "${RUSTC_WORKSPACE_WRAPPER:-}" ]]; then
  echo "composition qualification rejects ambient Rust compiler wrappers" >&2
  exit 1
fi
if grep -Eq '^(DIVAN_|CARGO_PROFILE_|RCA_COMPOSITION_ALLOCATION_)' < <(env); then
  echo "composition qualification rejects ambient benchmark overrides" >&2
  exit 1
fi
cargo_bin=$(rustup which --toolchain 1.98.0 cargo)
rustc_bin=$(rustup which --toolchain 1.98.0 rustc)
rustdoc_bin=$(rustup which --toolchain 1.98.0 rustdoc)
toolchain_dir=${cargo_bin%/*}
if [[ "$(${rustc_bin} --version)" != rustc\ 1.98.0* \
  || "$(${cargo_bin} --version)" != cargo\ 1.98.0* \
  || "$(${rustdoc_bin} --version)" != rustdoc\ 1.98.0* \
  || "${rustc_bin}" != "${toolchain_dir}/rustc" \
  || "${rustdoc_bin}" != "${toolchain_dir}/rustdoc" ]]; then
  echo "composition qualification requires Cargo, rustc, and rustdoc 1.98.0 from one toolchain" >&2
  exit 1
fi

export CARGO="${cargo_bin}"
export RUSTC="${rustc_bin}"
export RUSTDOC="${rustdoc_bin}"
export RUSTC_WRAPPER=
export RUSTC_WORKSPACE_WRAPPER=
export DYLD_LIBRARY_PATH="${toolchain_dir%/bin}/lib"
objcopy_bin="$(${rustc_bin} --print target-libdir)/../bin/rust-objcopy"
if ! "${objcopy_bin}" --version >/dev/null 2>&1; then
  echo "pinned rust-objcopy must run before binary-size qualification" >&2
  exit 1
fi
export PATH="${repo_root}/target/task4-tools/bin:${toolchain_dir}:${PATH}"
if [[ ! -x "${repo_root}/target/task4-tools/bin/cargo-bloat" \
  || "$(cargo bloat --version 2>/dev/null)" != "0.12.1" ]]; then
  echo "pinned cargo-bloat 0.12.1 is required under target/task4-tools" >&2
  exit 1
fi
python3 "${evaluator}" validate-allocation-manifest \
  --manifest "${allocation_manifest}" >/dev/null

output_dir=$1
if [[ -e "${output_dir}" ]]; then
  echo "raw output directory already exists: ${output_dir}" >&2
  exit 1
fi

temporary=$(mktemp -d /tmp/rca-composition-qualification.XXXXXX)
cleanup() {
  case "${temporary}" in
    /tmp/rca-composition-qualification.*) rm -R -- "${temporary}" ;;
    *) echo "refusing to remove unexpected temporary path: ${temporary}" >&2 ;;
  esac
}
trap cleanup EXIT

mkdir -p "${output_dir}"
export CARGO_BUILD_JOBS=1
export CARGO_PROFILE_BENCH_DEBUG=false
export CARGO_PROFILE_BENCH_LTO=false
export CARGO_PROFILE_BENCH_STRIP=true

sanitize() {
  sed \
    -e "s|${repo_root}|<repo>|g" \
    -e "s|${temporary}|<tmp>|g" \
    -e "s|${toolchain_dir}|<toolchain>|g" \
    -e "s|${HOME}|<home>|g" \
    | awk '
      {
        sub(/[[:space:]]+$/, "")
        if ($0 == "") {
          pending = pending "\n"
          next
        }
        printf "%s", pending
        pending = ""
        print
      }
    '
}

capture() {
  local destination=$1
  shift
  local raw_log="${temporary}/capture.log"
  if "$@" >"${raw_log}" 2>&1; then
    sanitize <"${raw_log}" >"${destination}"
  else
    local status=$?
    sanitize <"${raw_log}" >"${destination}"
    return "${status}"
  fi
}

validate_verbose_toolchain() {
  local transcript=$1
  if ! grep -Fq 'RUSTC=<toolchain>/rustc' "${transcript}" \
    || ! grep -Fq 'RUSTDOC=<toolchain>/rustdoc' "${transcript}" \
    || grep -Fq 'RUSTC=rustc' "${transcript}" \
    || grep -Fq 'RUSTDOC=rustdoc' "${transcript}" \
    || grep -Eq '/opt/homebrew/(Cellar|opt)/rust' "${transcript}"; then
    echo "verbose Cargo transcript did not prove the pinned Rust toolchain: ${transcript}" >&2
    exit 1
  fi
}

revision=$(git rev-parse HEAD)
recorded_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
{
  echo "measured_revision=${revision}"
  echo "recorded_at=${recorded_at}"
  echo "dirty_state=clean"
  echo "architecture=$(uname -m)"
  echo "machine_model=$(sysctl -n hw.model)"
  echo "cpu_model=$(sysctl -n machdep.cpu.brand_string)"
  echo "logical_cpus=$(sysctl -n hw.logicalcpu)"
  echo "memory_bytes=$(sysctl -n hw.memsize)"
  echo "os_version=$(sw_vers -productVersion)"
  echo "os_build=$(sw_vers -buildVersion)"
  if command -v pmset >/dev/null 2>&1; then
    echo "power_source=$(pmset -g batt | sed -n '1p')"
    echo "power_mode=$(pmset -g custom | sed -n '/^[[:space:]]*powermode[[:space:]]/p' | tr '\n' ';')"
  else
    echo "power_source=unavailable"
    echo "power_mode=unavailable"
  fi
  echo "cargo_version=$(${cargo_bin} --version)"
  echo "cargo_path=${cargo_bin}"
  echo "rustc_path=${rustc_bin}"
  echo "rustdoc_path=${rustdoc_bin}"
  echo "objcopy_path=${objcopy_bin}"
  echo "objcopy_version=$(${objcopy_bin} --version | tr '\n' ';')"
  echo "dyld_library_path=${DYLD_LIBRARY_PATH}"
  echo "rustc_wrapper=disabled"
  echo "rustc_workspace_wrapper=disabled"
  echo "rustc_verbose_begin"
  "${rustc_bin}" --version --verbose
  echo "rustc_verbose_end"
  echo "rustdoc_verbose_begin"
  "${rustdoc_bin}" --version --verbose
  echo "rustdoc_verbose_end"
  echo "cargo_bloat_version=$(cargo bloat --version)"
  echo "root_lock_sha256=$(shasum -a 256 Cargo.lock | sed 's/[[:space:]].*//')"
  echo "fixture_lock_sha256=$(shasum -a 256 benches/compile/Cargo.lock | sed 's/[[:space:]].*//')"
  echo "benchmark_source_sha256=$(shasum -a 256 rca-core/benches/composition.rs | sed 's/[[:space:]].*//')"
  echo "scope_zero_test_source_sha256=$(shasum -a 256 rca-core/src/composition/scope.rs | sed 's/[[:space:]].*//')"
  echo "combine_zero_test_source_sha256=$(shasum -a 256 rca-core/src/composition/combine.rs | sed 's/[[:space:]].*//')"
  echo "runner_source_sha256=${runner_source_sha256}"
  echo "allocation_manifest_sha256=$(shasum -a 256 "${allocation_manifest}" | sed 's/[[:space:]].*//')"
  echo "transcript_normalisation=paths,terminal-padding,trailing-blank-lines"
  echo "bench_profile_override=lto=false,debug=false,strip=true"
  echo "runtime_process_repetitions=10"
  echo "runtime_expected_timing_rows=153"
  echo "runtime_policy=ten independent full Divan processes from one clean committed executable"
  echo "zero_gate_profile=test,opt-level=1,debug=0; targeted fallback after default-profile linker SIGKILL"
  echo "compile_policy=one warm-up then five independent clean targets with one paired mtime-only incremental sample each"
  echo "fixture_binary_profile=release,opt-level=3,lto=fat,codegen-units=1,panic=abort,strip=symbols"
  echo "root_binary_profile=release,opt-level=3-default,lto=fat,codegen-units=1,panic=unwind-default,strip=true"
  echo "symbol_profile=release with strip=false only; cargo-bloat 0.12.1 crate attribution"
  echo "monomorphisation_counter=unavailable on stable toolchain"
} | sanitize >"${output_dir}/environment.txt"

cat >"${output_dir}/commands.txt" <<'COMMANDS'
Runtime build: cargo bench -p rca-core --bench composition --locked -j 1 --no-run --message-format=json
Runtime samples: execute the built composition benchmark with --bench ten independent times (01 through 10)
Allocation: one direct execution per named scenario of the allocation-measurement benchmark binary
Zero gates: cargo test -p rca-core --features allocation-measurement <exact-test> --locked -j 1
Compile: /usr/bin/time -lp cargo check --manifest-path benches/compile/Cargo.toml -p <fixture> --locked -j 1
Binary: cargo build --release, plus cargo test --release --test app --no-run, with isolated targets
Symbols: cargo bloat 0.12.1 --release --locked --crates -n 20 -j 1 with strip=false
COMMANDS

normal_build="${temporary}/normal-build.jsonl"
"${cargo_bin}" bench -p rca-core --bench composition --locked -j 1 --no-run \
  --message-format=json >"${normal_build}"
normal_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "composition")
  | .executable // empty
' "${normal_build}" | tail -1)
if [[ ! -x "${normal_executable}" ]]; then
  echo "normal composition benchmark executable was not produced" >&2
  exit 1
fi
for repetition in 01 02 03 04 05 06 07 08 09 10; do
  runtime_transcript="${output_dir}/runtime-divan-${repetition}.txt"
  capture "${runtime_transcript}" "${normal_executable}" --bench
  timing_rows=$(grep -Ec '[0-9]+([.][0-9]+)? (ns|µs|ms|s)[[:space:]]+│' "${runtime_transcript}")
  if [[ "${timing_rows}" -ne 153 ]] \
    || ! grep -Fq "fastest" "${runtime_transcript}" \
    || ! grep -Fq "median" "${runtime_transcript}" \
    || ! grep -Eq 'scoped_concurrent_senders_32[[:space:]]+[0-9]' "${runtime_transcript}"; then
    echo "runtime transcript ${repetition} is incomplete or missing Divan timings" >&2
    exit 1
  fi
done

allocation_build="${temporary}/allocation-build.jsonl"
"${cargo_bin}" bench -p rca-core --bench composition --features allocation-measurement \
  --locked -j 1 --no-run --message-format=json >"${allocation_build}"
allocation_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "composition")
  | .executable // empty
' "${allocation_build}" | tail -1)
if [[ ! -x "${allocation_executable}" || "${allocation_executable}" == "${normal_executable}" ]]; then
  echo "distinct allocation benchmark executable was not produced" >&2
  exit 1
fi

capture "${output_dir}/zero-direct-scope.txt" \
  env CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_TEST_OPT_LEVEL=1 \
  "${cargo_bin}" test -p rca-core --lib --features allocation-measurement \
  composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up \
  --locked -j 1 -- --exact --nocapture
capture "${output_dir}/zero-combine.txt" \
  env CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_TEST_OPT_LEVEL=1 \
  "${cargo_bin}" test -p rca-core --lib --features allocation-measurement \
  composition::combine::tests::combine_all_none_widths_do_not_allocate \
  --locked -j 1 -- --exact --nocapture

while read -r scenario samples logical_operations; do
  [[ -n "${scenario}" ]] || continue
  allocation_row=$(RCA_COMPOSITION_ALLOCATION_SCENARIO="${scenario}" \
    RCA_COMPOSITION_ALLOCATION_SAMPLES="${samples}" \
    "${allocation_executable}")
  expected_prefix="scenario=${scenario} samples=${samples} logical_operations=${logical_operations} "
  if [[ "${allocation_row}" != "${expected_prefix}"* ]]; then
    echo "allocation workload output mismatch: ${scenario}" >&2
    exit 1
  fi
  printf '%s\n' "${allocation_row}" >>"${output_dir}/allocations.txt"
done <"${allocation_manifest}"

while read -r fixture package source; do
  warm_target="${temporary}/compile/${fixture}/warm"
  capture "${output_dir}/compile-${fixture}-rustc-args.txt" \
    env CARGO_TARGET_DIR="${warm_target}" CARGO_INCREMENTAL=1 \
    "${cargo_bin}" check --manifest-path benches/compile/Cargo.toml \
    -p "${package}" --locked -j 1 -vv
  validate_verbose_toolchain "${output_dir}/compile-${fixture}-rustc-args.txt"
  : >"${output_dir}/compile-${fixture}.txt"
  for sample in 1 2 3 4 5; do
    sample_target="${temporary}/compile/${fixture}/sample-${sample}"
    echo "sample=${sample} phase=clean" >>"${output_dir}/compile-${fixture}.txt"
    capture "${temporary}/timed.txt" /usr/bin/time -lp env \
      CARGO_TARGET_DIR="${sample_target}" CARGO_INCREMENTAL=1 \
      "${cargo_bin}" check --manifest-path benches/compile/Cargo.toml \
      -p "${package}" --locked -j 1
    cat "${temporary}/timed.txt" >>"${output_dir}/compile-${fixture}.txt"
    touch "${source}"
    echo "sample=${sample} phase=incremental" >>"${output_dir}/compile-${fixture}.txt"
    capture "${temporary}/timed.txt" /usr/bin/time -lp env \
      CARGO_TARGET_DIR="${sample_target}" CARGO_INCREMENTAL=1 \
      "${cargo_bin}" check --manifest-path benches/compile/Cargo.toml \
      -p "${package}" --locked -j 1
    cat "${temporary}/timed.txt" >>"${output_dir}/compile-${fixture}.txt"
  done
done <<'FIXTURES'
direct rca-compile-direct benches/compile/direct/src/main.rs
composed-manual rca-compile-composed-manual benches/compile/composed-manual/src/main.rs
composed-macro rca-compile-composed-macro benches/compile/composed-macro/src/main.rs
scoped rca-compile-scoped benches/compile/scoped/src/main.rs
FIXTURES

fixture_binary_target="${temporary}/binary-fixtures"
capture "${output_dir}/binary-fixture-build.txt" env CARGO_TARGET_DIR="${fixture_binary_target}" \
  "${cargo_bin}" build --manifest-path benches/compile/Cargo.toml --workspace \
  --release --locked -j 1
root_binary_target="${temporary}/binary-root"
capture "${output_dir}/binary-tokio-build.txt" env CARGO_TARGET_DIR="${root_binary_target}" \
  "${cargo_bin}" build -p rca-example-composition-search --release --locked -j 1
app_test_build="${temporary}/app-test-build.jsonl"
env CARGO_TARGET_DIR="${root_binary_target}" "${cargo_bin}" test \
  -p rca-example-composition-search --test app --release --locked -j 1 \
  --no-run --message-format=json >"${app_test_build}"
app_test_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "app")
  | .executable // empty
' "${app_test_build}" | tail -1)

: >"${output_dir}/binary-sizes.txt"
record_binary() {
  local label=$1
  local binary=$2
  if [[ ! -x "${binary}" ]]; then
    echo "binary was not produced: ${label}" >&2
    exit 1
  fi
  # Cargo may succeed after its stripping helper fails; qualify the artifact itself.
  if ! "${objcopy_bin}" --strip-all "${binary}" >"${output_dir}/strip-${label}.txt" 2>&1; then
    echo "binary stripping failed: ${label}" >&2
    return 1
  fi
  {
    echo "binary=${label}"
    echo "bytes=$(stat -f '%z' "${binary}")"
    echo "sha256=$(shasum -a 256 "${binary}" | sed 's/[[:space:]].*//')"
    file -b "${binary}" | sed 's/^/identity=/'
  } >>"${output_dir}/binary-sizes.txt"
}

record_binary direct "${fixture_binary_target}/release/rca-compile-direct"
record_binary composed-manual "${fixture_binary_target}/release/rca-compile-composed-manual"
record_binary composed-macro "${fixture_binary_target}/release/rca-compile-composed-macro"
record_binary scoped "${fixture_binary_target}/release/rca-compile-scoped"
record_binary deterministic-composition-test "${app_test_executable}"
record_binary tokio-application "${root_binary_target}/release/rca-example-composition-search"

export CARGO_PROFILE_RELEASE_STRIP=false
while read -r fixture package; do
  capture "${output_dir}/symbols-${fixture}.txt" env \
    CARGO_TARGET_DIR="${temporary}/symbols-fixtures" \
    cargo bloat --manifest-path benches/compile/Cargo.toml -p "${package}" \
    --bin "${package}" --release --locked --crates -n 20 -j 1
done <<'FIXTURE_SYMBOLS'
direct rca-compile-direct
composed-manual rca-compile-composed-manual
composed-macro rca-compile-composed-macro
scoped rca-compile-scoped
FIXTURE_SYMBOLS
capture "${output_dir}/symbols-deterministic-composition-test.txt" env \
  CARGO_TARGET_DIR="${temporary}/symbols-root" \
  cargo bloat -p rca-example-composition-search --test app --release --locked \
  --crates -n 20 -j 1
capture "${output_dir}/symbols-tokio-application.txt" env \
  CARGO_TARGET_DIR="${temporary}/symbols-root" \
  cargo bloat -p rca-example-composition-search \
  --bin rca-example-composition-search --release --locked --crates -n 20 -j 1

echo "completed_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)" >>"${output_dir}/environment.txt"
