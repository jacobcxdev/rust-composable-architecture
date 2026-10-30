#!/usr/bin/env bash
set -euo pipefail

require_reference_object() {
  if ! git -C "$1" cat-file -e "$2^{commit}" 2>/dev/null; then
    echo "archived reference object is missing from the controller repository" >&2
    return 1
  fi
}

# The separate index lets Git check exported files without touching either checkout.
check_staged_source() {
  local source=$1 revision=$2 lock_hash=$3 harness_hash=$4 destination=$5
  if [[ -L "${destination}/Cargo.lock" || -L "${destination}/rca-tokio/benches/qualification.rs" \
    || "$(shasum -a 256 "${destination}/Cargo.lock" | awk '{print $1}')" != "${lock_hash}" \
    || "$(shasum -a 256 "${destination}/rca-tokio/benches/qualification.rs" | awk '{print $1}')" != "${harness_hash}" ]] \
    || ! GIT_INDEX_FILE="${destination}.index" git -C "${source}" --work-tree="${destination}" \
      diff --quiet "${revision}" -- . ':(exclude)Cargo.lock' ':(exclude)rca-tokio/benches/qualification.rs' \
    || [[ -n "$(GIT_INDEX_FILE="${destination}.index" git -C "${source}" --work-tree="${destination}" \
      ls-files --others --exclude-standard -- . ':(exclude)Cargo.lock')" ]]; then
    echo "staged source changed outside the common harness and pinned lockfile" >&2
    return 1
  fi
}

# Only the benchmark source is replaced. Library source and manifests stay exact.
stage_source() {
  local source=$1 revision=$2 tree=$3 lock=$4 lock_hash=$5 harness=$6 harness_hash=$7 destination=$8
  if [[ "$(git -C "${source}" rev-parse "${revision}^{tree}")" != "${tree}" ]]; then
    echo "source tree does not match the recorded revision" >&2
    return 1
  fi
  if [[ "$(shasum -a 256 "${lock}" | awk '{print $1}')" != "${lock_hash}" \
    || "$(shasum -a 256 "${harness}" | awk '{print $1}')" != "${harness_hash}" ]]; then
    echo "staging input hash mismatch" >&2
    return 1
  fi
  if git -C "${source}" cat-file -e "${revision}:Cargo.lock" 2>/dev/null \
    && [[ "$(git -C "${source}" show "${revision}:Cargo.lock" | shasum -a 256 | awk '{print $1}')" != "${lock_hash}" ]]; then
    echo "tracked source lockfile differs from the supplied lockfile" >&2
    return 1
  fi
  if git -C "${source}" ls-tree -r "${revision}" | awk '
    $1 == "120000" || $1 == "160000" {found = 1} END {exit !found}
  '; then
    echo "source tree contains a link or submodule" >&2
    return 1
  fi
  if [[ -e "${destination}" || -L "${destination}" \
    || -e "${destination}.index" || -L "${destination}.index" \
    || -e "${destination}.tar" || -L "${destination}.tar" ]]; then
    echo "staging destination already exists" >&2
    return 1
  fi
  mkdir "${destination}"
  git -C "${source}" archive --output="${destination}.tar" "${revision}"
  tar -x -f "${destination}.tar" -C "${destination}"
  rm -- "${destination}.tar"
  GIT_INDEX_FILE="${destination}.index" git -C "${source}" read-tree "${revision}"
  cp "${lock}" "${destination}/Cargo.lock"
  cp "${harness}" "${destination}/rca-tokio/benches/qualification.rs"
  check_staged_source "${source}" "${revision}" "${lock_hash}" "${harness_hash}" "${destination}"
}

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <candidate.json> <reference-worktree-or-revision> <reference.json>" >&2
  exit 2
fi

requested_output=$1
reference_input=$2
reference_root=
if [[ ! "${reference_input}" =~ ^[0-9a-f]{40}$ ]]; then
  reference_root=$(git -C "${reference_input}" rev-parse --show-toplevel)
fi
requested_reference_output=$3
output_dir=$(dirname "${requested_output}")
reference_output_dir=$(dirname "${requested_reference_output}")
canonical_runner=mac15-9-m3-max-16c-48gb-macos27-ac-high

for command in awk basename cmp cp env git grep jq mktemp pgrep rustup shasum tar tr; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command not found: ${command}" >&2
    exit 1
  fi
done

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "canonical qualification runner requires macOS" >&2
  exit 1
fi

for command in pmset stat sysctl sw_vers xcrun; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required macOS command not found: ${command}" >&2
    exit 1
  fi
done

if [[ -n "$(git status --porcelain=v1 --untracked-files=all)" ]]; then
  echo "performance qualification requires a clean candidate checkout" >&2
  exit 1
fi
if [[ -n "${reference_root}" \
  && -n "$(git -C "${reference_root}" status --porcelain=v1 --untracked-files=all)" ]]; then
  echo "performance qualification requires a clean reference checkout" >&2
  exit 1
fi

if [[ -e "${requested_output}" || -e "${requested_reference_output}" \
  || -e "${requested_output%.json}-profile.md" \
  || -e "${requested_output%.json}-divan.txt" ]]; then
  echo "qualification evidence already exists; choose a new result path" >&2
  exit 1
fi

mkdir -p "${output_dir}" "${reference_output_dir}"
output=$(cd "${output_dir}" && pwd -P)/$(basename "${requested_output}")
reference_output=$(cd "${reference_output_dir}" && pwd -P)/$(basename "${requested_reference_output}")
profile_output=${output%.json}-profile.md
divan_output=${output%.json}-divan.txt
if [[ "${output}" == "${reference_output}" \
  || "${output}" == "${profile_output}" \
  || "${output}" == "${divan_output}" \
  || "${reference_output}" == "${profile_output}" \
  || "${reference_output}" == "${divan_output}" \
  || "${profile_output}" == "${divan_output}" ]]; then
  echo "qualification evidence paths must be distinct" >&2
  exit 1
fi
if [[ -e "${output}" || -e "${reference_output}" \
  || -e "${profile_output}" || -e "${divan_output}" ]]; then
  echo "qualification evidence already exists after path resolution" >&2
  exit 1
fi

if [[ -n "${RCA_QUALIFY_ALLOCATIONS:-}" || -n "${RCA_DHAT_FILE:-}" ]]; then
  echo "RCA allocation profiler variables must be unset before qualification" >&2
  exit 1
fi

if pgrep -f 'target/(debug|release)/deps/(qualification|effects|observation|store_actor|deterministic|runtime)-|cargo bench' >/dev/null 2>&1; then
  echo "another RCA benchmark process is running" >&2
  exit 1
fi

runner=${RCA_PERF_RUNNER:-${canonical_runner}}
if [[ "${runner}" != "${canonical_runner}" ]]; then
  echo "runner must be pinned to ${canonical_runner}" >&2
  exit 1
fi

machine_model=$(sysctl -n hw.model)
cpu_model=$(sysctl -n machdep.cpu.brand_string)
logical_cpus=$(sysctl -n hw.logicalcpu)
memory_bytes=$(sysctl -n hw.memsize)
os_version=$(sw_vers -productVersion)
os_build=$(sw_vers -buildVersion)
power_source=$(pmset -g batt | awk 'NR == 1 {print}')
power_mode=$(pmset -g custom | awk '
  $1 == "AC" && $2 == "Power:" {in_ac = 1; next}
  in_ac && $1 == "powermode" {print $2; exit}
')

if [[ "$(uname -m)" != "arm64" \
  || "${machine_model}" != "Mac15,9" \
  || "${cpu_model}" != "Apple M3 Max" \
  || "${logical_cpus}" != "16" \
  || "${memory_bytes}" != "51539607552" \
  || "${os_version%%.*}" != "27" \
  || "${power_source}" != *"AC Power"* \
  || "${power_mode}" != "2" ]]; then
  echo "host does not match pinned runner ${canonical_runner}" >&2
  exit 1
fi

cargo=$(rustup which --toolchain 1.98.0 cargo)
rustc=$(rustup which --toolchain 1.98.0 rustc)
toolchain_bin=$(dirname "${cargo}")
toolchain_root=$(dirname "${toolchain_bin}")
toolchain_library_path=${toolchain_root}/lib
build_library_path=${toolchain_library_path}
if [[ -n "${DYLD_LIBRARY_PATH:-}" ]]; then
  build_library_path=${build_library_path}:${DYLD_LIBRARY_PATH}
fi
export PATH="${toolchain_bin}:${PATH}"
export RUSTC="${rustc}"

rustc_version=$("${rustc}" --version)
cargo_version=$("${cargo}" --version)
if [[ "${rustc_version}" != rustc\ 1.98.0* || "${cargo_version}" != cargo\ 1.98.0* ]]; then
  echo "qualification requires exact Rust and Cargo 1.98.0" >&2
  exit 1
fi

if ! grep -q '^\[profile.release\]$' Cargo.toml \
  || ! grep -q '^codegen-units = 1$' Cargo.toml \
  || ! grep -q '^lto = "fat"$' Cargo.toml; then
  echo "canonical release profile is missing" >&2
  exit 1
fi
repo_root=$(pwd -P)
revision=$(git rev-parse HEAD)
reference_repository=${reference_root:-${repo_root}}
reference_revision=${reference_input}
if [[ -n "${reference_root}" ]]; then
  reference_revision=$(git -C "${reference_root}" rev-parse HEAD)
fi
if [[ "${revision}" == "${reference_revision}" ]]; then
  echo "candidate and reference revisions must differ" >&2
  exit 1
fi
if ! grep -Eq '^[0-9a-f]{40}$' docs/performance/baseline-revision.txt; then
  echo "performance baseline revision pin is invalid" >&2
  exit 1
fi
expected_reference_revision=$(tr -d '\n' < docs/performance/baseline-revision.txt)
if [[ "${reference_revision}" != "${expected_reference_revision}" ]]; then
  echo "reference does not match performance baseline revision" >&2
  exit 1
fi
require_reference_object "${repo_root}" "${reference_revision}"
git -C "${reference_repository}" cat-file -e "${reference_revision}^{commit}"
reference_manifest=$(git -C "${reference_repository}" show "${reference_revision}:Cargo.toml")
if ! grep -q '^\[profile.release\]$' <<< "${reference_manifest}" \
  || ! grep -q '^codegen-units = 1$' <<< "${reference_manifest}" \
  || ! grep -q '^lto = "fat"$' <<< "${reference_manifest}"; then
  echo "reference release profile is missing" >&2
  exit 1
fi
if ! git ls-files --error-unmatch Cargo.lock >/dev/null 2>&1; then
  echo "candidate Cargo.lock must be tracked" >&2
  exit 1
fi
if [[ ! -f docs/performance/baseline-Cargo.lock \
  || ! -f docs/performance/scenarios.txt ]]; then
  echo "candidate and reference lockfiles are required" >&2
  exit 1
fi
if [[ -n "${reference_root}" ]] \
  && ! cmp -s "${reference_root}/Cargo.lock" docs/performance/baseline-Cargo.lock; then
  echo "reference Cargo.lock does not match pinned baseline lockfile" >&2
  exit 1
fi
candidate_lock_sha256=$(shasum -a 256 Cargo.lock | awk '{print $1}')
reference_lock_sha256=$(shasum -a 256 docs/performance/baseline-Cargo.lock | awk '{print $1}')
candidate_source_tree=$(git rev-parse "${revision}^{tree}")
reference_source_tree=$(git -C "${reference_repository}" rev-parse "${reference_revision}^{tree}")
candidate_manifest_sha256=$(git show "${revision}:Cargo.toml" | shasum -a 256 | awk '{print $1}')
reference_manifest_sha256=$(git -C "${reference_repository}" show "${reference_revision}:Cargo.toml" \
  | shasum -a 256 | awk '{print $1}')
candidate_original_harness_sha256=$(git show "${revision}:rca-tokio/benches/qualification.rs" \
  | shasum -a 256 | awk '{print $1}')
reference_original_harness_sha256=$(git -C "${reference_repository}" \
  show "${reference_revision}:rca-tokio/benches/qualification.rs" | shasum -a 256 | awk '{print $1}')
qualification_source_sha256=$(shasum -a 256 rca-tokio/benches/qualification.rs | awk '{print $1}')
scenario_manifest_sha256=$(shasum -a 256 docs/performance/scenarios.txt | awk '{print $1}')
qualification_script_sha256=$(shasum -a 256 scripts/qualify-performance.sh | awk '{print $1}')
protocol_sha256=$(printf '%s\n' "${qualification_source_sha256}" \
  "${scenario_manifest_sha256}" "${qualification_script_sha256}" \
  | shasum -a 256 | awk '{print $1}')
recorded_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)
rustc_commit=$("${rustc}" --version --verbose | awk '/^commit-hash:/ {print $2}')
if [[ "${repo_root}" == *'"'* ]]; then
  echo "repository path cannot contain a quote" >&2
  exit 1
fi
umbrella_package=$("${cargo}" metadata --locked --no-deps --format-version 1 \
  | jq -r --arg manifest "${repo_root}/rca/Cargo.toml" '
      .packages[] | select(.manifest_path == $manifest) | .name
    ')
case "${umbrella_package}" in
  rca|rust-composable-architecture) ;;
  *)
    echo "unsupported RCA umbrella package: ${umbrella_package}" >&2
    exit 1
    ;;
esac

temporary=$(mktemp -d /tmp/rca-qualification.XXXXXX)
cohort_id=$(printf '%s\n' "${revision}" "${reference_revision}" "${recorded_at}" \
  "$(basename "${temporary}")" | shasum -a 256 | awk '{print $1}')
profile_report=${temporary}/candidate-profile.md
divan_log=${temporary}/candidate-divan.txt
completed=0
output_published=0
reference_output_published=0
profile_output_published=0
divan_output_published=0
stage=initialisation
cleanup() {
  if [[ "${completed}" -eq 0 ]]; then
    [[ "${output_published}" -eq 0 ]] || rm -f -- "${output}"
    [[ "${reference_output_published}" -eq 0 ]] || rm -f -- "${reference_output}"
    [[ "${profile_output_published}" -eq 0 ]] || rm -f -- "${profile_output}"
    [[ "${divan_output_published}" -eq 0 ]] || rm -f -- "${divan_output}"
  fi
  case "${temporary}" in
    /tmp/rca-qualification.*) rm -R -- "${temporary}" ;;
    *) echo "refusing to remove unexpected temporary path: ${temporary}" >&2 ;;
  esac
}
trap cleanup EXIT
trap 'echo "qualification failed during ${stage} at line ${LINENO}" >&2' ERR

stage="source-staging"
candidate_build_root=${temporary}/candidate-source
reference_build_root=${temporary}/reference-source
stage_source "${repo_root}" "${revision}" "${candidate_source_tree}" \
  "${repo_root}/Cargo.lock" "${candidate_lock_sha256}" \
  "${repo_root}/rca-tokio/benches/qualification.rs" "${qualification_source_sha256}" "${candidate_build_root}"
stage_source "${reference_repository}" "${reference_revision}" "${reference_source_tree}" \
  "${repo_root}/docs/performance/baseline-Cargo.lock" "${reference_lock_sha256}" \
  "${repo_root}/rca-tokio/benches/qualification.rs" "${qualification_source_sha256}" "${reference_build_root}"
"${cargo}" metadata --locked --manifest-path "${reference_build_root}/Cargo.toml" \
  --no-deps --format-version 1 >/dev/null

normal_build=${temporary}/normal-build.jsonl
allocation_build=${temporary}/allocation-build.jsonl
stage=benchmark-build
"${cargo}" bench --locked --manifest-path "${candidate_build_root}/Cargo.toml" \
  --target-dir "${candidate_build_root}/target" \
  -p rca-tokio --bench qualification --no-run --message-format=json \
  > "${normal_build}"
normal_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "qualification")
  | .executable // empty
' "${normal_build}" | tail -1)

"${cargo}" bench --locked --manifest-path "${candidate_build_root}/Cargo.toml" \
  --target-dir "${candidate_build_root}/target" \
  -p rca-tokio --bench qualification --features qualification-dhat \
  --no-run --message-format=json > "${allocation_build}"
allocation_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "qualification")
  | .executable // empty
' "${allocation_build}" | tail -1)

reference_normal_build=${temporary}/reference-normal-build.jsonl
reference_allocation_build=${temporary}/reference-allocation-build.jsonl
"${cargo}" bench --locked --manifest-path "${reference_build_root}/Cargo.toml" \
  --target-dir "${reference_build_root}/target" \
  -p rca-tokio --bench qualification --no-run --message-format=json \
  > "${reference_normal_build}"
reference_normal_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "qualification")
  | .executable // empty
' "${reference_normal_build}" | tail -1)

"${cargo}" bench --locked --manifest-path "${reference_build_root}/Cargo.toml" \
  --target-dir "${reference_build_root}/target" \
  -p rca-tokio --bench qualification --features qualification-dhat \
  --no-run --message-format=json > "${reference_allocation_build}"
reference_allocation_executable=$(jq -r '
  select(.reason == "compiler-artifact" and .target.name == "qualification")
  | .executable // empty
' "${reference_allocation_build}" | tail -1)

if [[ ! -x "${normal_executable}" || ! -x "${allocation_executable}" \
  || ! -x "${reference_normal_executable}" \
  || ! -x "${reference_allocation_executable}" ]]; then
  echo "qualification executable was not produced" >&2
  exit 1
fi
if [[ "${normal_executable}" == "${allocation_executable}" \
  || "${reference_normal_executable}" == "${reference_allocation_executable}" \
  || "${normal_executable}" == "${reference_normal_executable}" \
  || "${allocation_executable}" == "${reference_allocation_executable}" ]]; then
  echo "candidate, reference, normal, and DHAT builds must be distinct" >&2
  exit 1
fi

check_staged_source "${repo_root}" "${revision}" "${candidate_lock_sha256}" \
  "${qualification_source_sha256}" "${candidate_build_root}"
check_staged_source "${reference_repository}" "${reference_revision}" "${reference_lock_sha256}" \
  "${qualification_source_sha256}" "${reference_build_root}"

scenarios=()
while IFS= read -r scenario; do
  scenarios[${#scenarios[@]}]="${scenario}"
done < <("${normal_executable}" --list)
if [[ ${#scenarios[@]} -ne 50 ]]; then
  echo "expected 50 qualification scenarios, found ${#scenarios[@]}" >&2
  exit 1
fi
actual_scenarios=${temporary}/scenarios.txt
printf '%s\n' "${scenarios[@]}" > "${actual_scenarios}"
if ! cmp -s "${actual_scenarios}" docs/performance/scenarios.txt; then
  echo "qualification scenarios do not match pinned manifest" >&2
  exit 1
fi
reference_scenarios=()
while IFS= read -r scenario; do
  reference_scenarios[${#reference_scenarios[@]}]="${scenario}"
done < <("${reference_normal_executable}" --list)
if [[ ${#reference_scenarios[@]} -ne ${#scenarios[@]} ]]; then
  echo "candidate and reference scenario counts differ" >&2
  exit 1
fi
for ((scenario_index = 0; scenario_index < ${#scenarios[@]}; scenario_index++)); do
  if [[ "${scenarios[scenario_index]}" != "${reference_scenarios[scenario_index]}" ]]; then
    echo "candidate and reference scenarios differ" >&2
    exit 1
  fi
done

rows=${temporary}/rows.jsonl
reference_rows=${temporary}/reference-rows.jsonl
: > "${rows}"
: > "${reference_rows}"
stage=qualification-matrix
normal_repetitions=10
allocation_repetitions=6

record_normal() {
  local executable=$1
  local scenario=$2
  local samples=$3
  local stem=$4
  local destination=$5
  local repetition=$6
  local order=$7
  local result=${temporary}/${stem}.json
  local timing=${temporary}/${stem}-time.txt
  local cpu_cycles
  local instructions
  local peak_resident
  local peak_footprint

  /usr/bin/time -lp -o "${timing}" \
    "${executable}" "${scenario}" "${samples}" > "${result}"
  cpu_cycles=$(awk '/cycles elapsed$/ {print $1; exit}' "${timing}")
  instructions=$(awk '/instructions retired$/ {print $1; exit}' "${timing}")
  peak_resident=$(awk '/maximum resident set size$/ {print $1; exit}' "${timing}")
  peak_footprint=$(awk '/peak memory footprint$/ {print $1; exit}' "${timing}")
  jq -c \
    --argjson cpu_cycles "${cpu_cycles:-0}" \
    --argjson instructions_retired "${instructions:-0}" \
    --argjson peak_resident_bytes "${peak_resident:-0}" \
    --argjson peak_memory_footprint_bytes "${peak_footprint:-0}" \
    --argjson repetition "${repetition}" \
    --arg order "${order}" '
      .cpu_cycles = $cpu_cycles
      | .instructions_retired = $instructions_retired
      | .peak_resident_bytes = $peak_resident_bytes
      | .peak_memory_footprint_bytes = $peak_memory_footprint_bytes
      | .repetition = $repetition
      | .order = $order
    ' "${result}" >> "${destination}"
}

record_allocation() {
  local executable=$1
  local scenario=$2
  local samples=$3
  local stem=$4
  local destination=$5
  local repetition=$6
  local order=$7
  local result=${temporary}/${stem}.json
  local profile=${temporary}/${stem}-dhat.json
  local log=${temporary}/${stem}-dhat.log

  RCA_QUALIFY_ALLOCATIONS=1 RCA_DHAT_FILE="${profile}" \
    "${executable}" "${scenario}" "${samples}" \
    > "${result}" 2> "${log}"
  jq -c --argjson repetition "${repetition}" --arg order "${order}" '
    .repetition = $repetition | .order = $order
  ' "${result}" >> "${destination}"
}

aggregate_normal() {
  jq -s '
    def median:
      sort as $values
      | ($values | length) as $count
      | if ($count % 2) == 1 then
          $values[$count / 2 | floor]
        else
          (($values[$count / 2 - 1] + $values[$count / 2]) / 2)
        end;
    . as $runs
    | $runs[0]
    | .throughput_per_second = ([$runs[].throughput_per_second] | median)
    | .latency_ns.p50 = ([$runs[].latency_ns.p50] | median)
    | .latency_ns.p95 = ([$runs[].latency_ns.p95] | median)
    | .latency_ns.p99 = ([$runs[].latency_ns.p99] | median)
    | .task_wakes = ([$runs[].task_wakes] | median)
    | .cpu_cycles = ([$runs[].cpu_cycles] | median)
    | .instructions_retired = ([$runs[].instructions_retired] | median)
    | .peak_resident_bytes = ([$runs[].peak_resident_bytes] | median)
    | .peak_memory_footprint_bytes = ([$runs[].peak_memory_footprint_bytes] | median)
  ' "$1" > "$2"
}

aggregate_allocation() {
  jq -s '
    def median:
      sort as $values
      | ($values | length) as $count
      | if ($count % 2) == 1 then
          $values[$count / 2 | floor]
        else
          (($values[$count / 2 - 1] + $values[$count / 2]) / 2)
        end;
    . as $runs
    | $runs[0]
    | .allocations = ([$runs[].allocations] | median)
    | .allocated_bytes = ([$runs[].allocated_bytes] | median)
    | .peak_bytes = ([$runs[].peak_bytes] | median)
  ' "$1" > "$2"
}

append_row() {
  local normal=$1
  local allocation=$2
  local pairs=$3
  local row_revision=$4
  local destination=$5
  jq -cn \
    --slurpfile normal "${normal}" \
    --slurpfile allocation "${allocation}" \
    --slurpfile pairs "${pairs}" \
    --arg revision "${row_revision}" \
    --arg runner "${runner}" \
    --argjson normal_repetitions "${normal_repetitions}" \
    --argjson allocation_repetitions "${allocation_repetitions}" '
      $normal[0]
      | del(.repetition, .order)
      | .revision = $revision
      | .runner = $runner
      | .allocations = $allocation[0].allocations
      | .allocation_samples = $allocation[0].samples
      | .allocation_operations = $allocation[0].operations
      | .allocated_bytes = $allocation[0].allocated_bytes
      | .peak_bytes = $allocation[0].peak_bytes
      | .normal_repetitions = $normal_repetitions
      | .allocation_repetitions = $allocation_repetitions
      | .pairs = $pairs[0]
      | .rationale_id = null
    ' >> "${destination}"
}

scenario_index=0
for scenario in "${scenarios[@]}"; do
  samples=10000
  allocation_samples=1000
  case "${scenario}" in
    */1000) allocation_samples=200 ;;
    deterministic/budget)
      allocation_samples=5
      ;;
    mailbox/*|shutdown/*|*/64) allocation_samples=500 ;;
  esac

  normal_runs=${temporary}/candidate-normal-${scenario_index}.jsonl
  allocation_runs=${temporary}/candidate-allocation-${scenario_index}.jsonl
  reference_normal_runs=${temporary}/reference-normal-${scenario_index}.jsonl
  reference_allocation_runs=${temporary}/reference-allocation-${scenario_index}.jsonl
  : > "${normal_runs}"
  : > "${allocation_runs}"
  : > "${reference_normal_runs}"
  : > "${reference_allocation_runs}"

  for ((repetition = 1; repetition <= normal_repetitions; repetition++)); do
    if ((repetition % 2 == 1)); then
      order=reference-candidate
      record_normal "${reference_normal_executable}" "${scenario}" "${samples}" \
        "reference-normal-${scenario_index}-${repetition}" "${reference_normal_runs}" \
        "${repetition}" "${order}"
      record_normal "${normal_executable}" "${scenario}" "${samples}" \
        "candidate-normal-${scenario_index}-${repetition}" "${normal_runs}" \
        "${repetition}" "${order}"
    else
      order=candidate-reference
      record_normal "${normal_executable}" "${scenario}" "${samples}" \
        "candidate-normal-${scenario_index}-${repetition}" "${normal_runs}" \
        "${repetition}" "${order}"
      record_normal "${reference_normal_executable}" "${scenario}" "${samples}" \
        "reference-normal-${scenario_index}-${repetition}" "${reference_normal_runs}" \
        "${repetition}" "${order}"
    fi
  done

  for ((repetition = 1; repetition <= allocation_repetitions; repetition++)); do
    if ((repetition % 2 == 1)); then
      order=reference-candidate
      record_allocation "${reference_allocation_executable}" "${scenario}" \
        "${allocation_samples}" "reference-allocation-${scenario_index}-${repetition}" \
        "${reference_allocation_runs}" "${repetition}" "${order}"
      record_allocation "${allocation_executable}" "${scenario}" \
        "${allocation_samples}" "candidate-allocation-${scenario_index}-${repetition}" \
        "${allocation_runs}" "${repetition}" "${order}"
    else
      order=candidate-reference
      record_allocation "${allocation_executable}" "${scenario}" \
        "${allocation_samples}" "candidate-allocation-${scenario_index}-${repetition}" \
        "${allocation_runs}" "${repetition}" "${order}"
      record_allocation "${reference_allocation_executable}" "${scenario}" \
        "${allocation_samples}" "reference-allocation-${scenario_index}-${repetition}" \
        "${reference_allocation_runs}" "${repetition}" "${order}"
    fi
  done

  normal_row=${temporary}/candidate-normal-${scenario_index}.json
  allocation_row=${temporary}/candidate-allocation-${scenario_index}.json
  reference_normal_row=${temporary}/reference-normal-${scenario_index}.json
  reference_allocation_row=${temporary}/reference-allocation-${scenario_index}.json
  aggregate_normal "${normal_runs}" "${normal_row}"
  aggregate_allocation "${allocation_runs}" "${allocation_row}"
  aggregate_normal "${reference_normal_runs}" "${reference_normal_row}"
  aggregate_allocation "${reference_allocation_runs}" "${reference_allocation_row}"
  pairs=${temporary}/pairs-${scenario_index}.json
  jq -n \
    --slurpfile candidate_normal "${normal_runs}" \
    --slurpfile reference_normal "${reference_normal_runs}" \
    --slurpfile candidate_allocation "${allocation_runs}" \
    --slurpfile reference_allocation "${reference_allocation_runs}" '
      if ($candidate_normal | length) != ($reference_normal | length)
        or ($candidate_allocation | length) != ($reference_allocation | length)
      then error("candidate and reference repetition counts differ")
      else {
        normal: [
          range(0; $candidate_normal | length) as $index
          | {
              repetition: $candidate_normal[$index].repetition,
              order: $candidate_normal[$index].order,
              reference: $reference_normal[$index],
              candidate: $candidate_normal[$index]
            }
        ],
        allocation: [
          range(0; $candidate_allocation | length) as $index
          | {
              repetition: $candidate_allocation[$index].repetition,
              order: $candidate_allocation[$index].order,
              reference: $reference_allocation[$index],
              candidate: $candidate_allocation[$index]
            }
        ]
      }
      end
    ' > "${pairs}"
  append_row "${normal_row}" "${allocation_row}" "${pairs}" "${revision}" "${rows}"
  append_row "${reference_normal_row}" "${reference_allocation_row}" "${pairs}" \
    "${reference_revision}" "${reference_rows}"

  scenario_index=$((scenario_index + 1))
done

expected_scenarios=$(jq -R -s -c 'split("\n") | map(select(length > 0))' \
  docs/performance/scenarios.txt)
validate_rows() {
  jq -s -e --argjson expected_scenarios "${expected_scenarios}" '
  def expected_samples:
    .samples == 10000;
  def expected_allocation_samples:
    if .scenario == "deterministic/budget" then .allocation_samples == 5
    elif (.scenario | endswith("/1000")) then .allocation_samples == 200
    elif ((.scenario | startswith("mailbox/"))
      or (.scenario | startswith("shutdown/"))
      or (.scenario | endswith("/64")))
      then .allocation_samples == 500
    else .allocation_samples == 1000
    end;
  def same_workload:
    .candidate.scenario == .reference.scenario
    and .candidate.samples == .reference.samples
    and .candidate.operations == .reference.operations
    and .candidate.repetition == .repetition
    and .reference.repetition == .repetition
    and .candidate.order == .order
    and .reference.order == .order;
  def valid_normal:
    same_workload
    and .candidate.throughput_per_second > 0
    and .reference.throughput_per_second > 0
    and .candidate.latency_ns.p50 > 0
    and .reference.latency_ns.p50 > 0
    and .candidate.latency_ns.p99 > 0
    and .reference.latency_ns.p99 > 0
    and .candidate.cpu_cycles > 0
    and .reference.cpu_cycles > 0
    and .candidate.task_wakes > 0
    and .reference.task_wakes > 0
    and .candidate.peak_resident_bytes > 0
    and .reference.peak_resident_bytes > 0
    and .candidate.peak_memory_footprint_bytes > 0
    and .reference.peak_memory_footprint_bytes > 0;
  def valid_allocation:
    same_workload
    and .candidate.allocations > 0
    and .reference.allocations > 0
    and .candidate.allocated_bytes > 0
    and .reference.allocated_bytes > 0
    and .candidate.peak_bytes > 0
    and .reference.peak_bytes > 0;
  length == 50
  and (map(.scenario) == $expected_scenarios)
  and all(.[];
    . as $row
    | expected_samples
    and expected_allocation_samples
    and .throughput_per_second > 0
    and .operations > 0
    and .latency_ns.p50 > 0
    and .latency_ns.p95 > 0
    and .latency_ns.p99 > 0
    and .cpu_cycles > 0
    and .allocations > 0
    and .allocation_samples > 0
    and .allocation_operations > 0
    and .allocated_bytes > 0
    and .task_wakes > 0
    and .peak_bytes > 0
    and .instructions_retired > 0
    and .peak_resident_bytes > 0
    and .peak_memory_footprint_bytes > 0
    and .normal_repetitions == 10
    and .allocation_repetitions == 6
    and (.pairs.normal | length) == 10
    and (.pairs.allocation | length) == 6
    and ([.pairs.normal[].repetition] == [1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
    and ([.pairs.allocation[].repetition] == [1, 2, 3, 4, 5, 6])
    and ([.pairs.normal[].order] == [
      "reference-candidate", "candidate-reference",
      "reference-candidate", "candidate-reference",
      "reference-candidate", "candidate-reference",
      "reference-candidate", "candidate-reference",
      "reference-candidate", "candidate-reference"
    ])
    and ([.pairs.allocation[].order] == [
      "reference-candidate", "candidate-reference",
      "reference-candidate", "candidate-reference",
      "reference-candidate", "candidate-reference"
    ])
    and all(.pairs.normal[];
      valid_normal
      and .candidate.scenario == $row.scenario
      and .candidate.samples == $row.samples
      and .candidate.operations == $row.operations
    )
    and all(.pairs.allocation[];
      valid_allocation
      and .candidate.scenario == $row.scenario
      and .candidate.samples == $row.allocation_samples
      and .candidate.operations == $row.allocation_operations
    )
  )
' "$1" >/dev/null
}
if ! validate_rows "${rows}" || ! validate_rows "${reference_rows}"; then
  echo "qualification matrix contains missing or zero metrics" >&2
  exit 1
fi

: > "${divan_log}"
stage=divan-suites
divan_benches=(
  rca-core:effects
  rca-core:observation
  rca-core:store_actor
  rca-test:deterministic
  rca-tokio:runtime
)
for specification in "${divan_benches[@]}"; do
  package=${specification%%:*}
  bench=${specification#*:}
  "${cargo}" bench --locked -p "${package}" --bench "${bench}" -- \
    --color never --sample-count 50 --max-time 0.5 >> "${divan_log}" 2>&1
done
divan_sha256=$(shasum -a 256 "${divan_log}" | awk '{print $1}')

compile_target=${temporary}/compile-target
compile_time=${temporary}/compile-time.txt
compile_log=${temporary}/compile.log
stage=clean-compile
/usr/bin/time -lp -o "${compile_time}" /usr/bin/env \
  DYLD_LIBRARY_PATH="${build_library_path}" CARGO_TARGET_DIR="${compile_target}" \
  "${cargo}" build --locked --workspace --all-features --release > "${compile_log}" 2>&1
if grep -q 'stripping debug info.*failed' "${compile_log}"; then
  echo "clean release build did not strip debug information" >&2
  exit 1
fi
compile_seconds=$(awk '/^real / {print $2; exit}' "${compile_time}")
compile_cycles=$(awk '/cycles elapsed$/ {print $1; exit}' "${compile_time}")
compile_peak_bytes=$(awk '/maximum resident set size$/ {print $1; exit}' "${compile_time}")

stage=binary-size
DYLD_LIBRARY_PATH="${build_library_path}" CARGO_TARGET_DIR="${temporary}/consumer-target" \
  "${cargo}" build --locked -p rca-size-probe --release \
  > "${temporary}/consumer.log" 2>&1
binary_size=$(stat -f '%z' "${temporary}/consumer-target/release/rca-size-probe")
qualification_binary_size=$(stat -f '%z' "${normal_executable}")

idle_ready=${temporary}/idle-ready.txt
stage=idle-memory
"${normal_executable}" --idle > "${idle_ready}" &
idle_pid=$!
ready=0
for _ in {1..100}; do
  if grep -q '^READY$' "${idle_ready}"; then
    ready=1
    break
  fi
  sleep 0.1
done
if [[ "${ready}" -ne 1 ]]; then
  kill -TERM "${idle_pid}" 2>/dev/null || true
  wait "${idle_pid}" 2>/dev/null || true
  echo "idle Store did not become ready" >&2
  exit 1
fi
sleep 1
idle_rss_kib=$(ps -o rss= -p "${idle_pid}" | awk '{print $1}')
kill -TERM "${idle_pid}" 2>/dev/null || true
wait "${idle_pid}" 2>/dev/null || true
idle_rss_bytes=$((idle_rss_kib * 1024))

profile_dir=target/qualification-profiles
stage=profiles
mkdir -p "${profile_dir}"
profile_stamp=$(date -u +%Y%m%dT%H%M%SZ)
cpu_trace=${profile_dir}/cpu-${revision:0:12}-${profile_stamp}.trace
contention_trace=${profile_dir}/contention-${revision:0:12}-${profile_stamp}.trace
heap_profile=${profile_dir}/heap-${revision:0:12}-${profile_stamp}.json
profile_log=${profile_dir}/profile-${revision:0:12}-${profile_stamp}.log

RCA_QUALIFY_ALLOCATIONS=1 RCA_DHAT_FILE="${heap_profile}" \
  "${allocation_executable}" effect/async 10000 \
  > "${profile_dir}/heap-${revision:0:12}-${profile_stamp}-row.json" \
  2> "${profile_log}"
capture_time_profile() {
  local trace=$1
  shift
  local exit_code=0
  if xcrun xctrace record --quiet --template 'Time Profiler' \
    --time-limit 3s --output "${trace}" --target-stdout /dev/null --launch -- "$@"; then
    exit_code=0
  else
    exit_code=$?
  fi
  if [[ "${exit_code}" -ne 0 && "${exit_code}" -ne 54 ]]; then
    echo "xctrace failed with status ${exit_code}" >&2
    return "${exit_code}"
  fi
  local toc
  toc=${temporary}/$(basename "${trace}").xml
  xcrun xctrace export --input "${trace}" --toc > "${toc}"
  grep -q 'schema="time-profile"' "${toc}"
  grep -q '<end-reason>Time limit reached</end-reason>' "${toc}"
}

capture_time_profile "${cpu_trace}" "${normal_executable}" \
  external_commit/multi/4_producers/saturated 100000
capture_time_profile "${contention_trace}" "${normal_executable}" \
  external_commit/multi/8_producers/saturated 100000

if [[ ! -d "${cpu_trace}" || ! -d "${contention_trace}" || ! -s "${heap_profile}" ]]; then
  echo "profiler evidence was not produced" >&2
  exit 1
fi

normal_command='balanced paired qualification <scenario> <samples> (10 repetitions, order-stratified paired ratio)'
allocation_command='balanced paired qualification-dhat <scenario> <samples> (6 repetitions, order-stratified paired ratio)'
divan_command='cargo bench --locked <phase bench> -- --sample-count 50 --max-time 0.5'
rows_array=${temporary}/rows.json
reference_rows_array=${temporary}/reference-rows.json
jq -s '.' "${rows}" > "${rows_array}"
jq -s '.' "${reference_rows}" > "${reference_rows_array}"
result=${temporary}/result.json
reference_result=${temporary}/reference-result.json
stage=result_assembly
jq -n \
  --slurpfile rows "${rows_array}" \
  --arg revision "${revision}" \
  --arg reference_revision "${reference_revision}" \
  --arg runner "${runner}" \
  --arg recorded_at "${recorded_at}" \
  --arg cohort_id "${cohort_id}" \
  --arg rustc "${rustc_version}" \
  --arg cargo "${cargo_version}" \
  --arg rustc_commit "${rustc_commit}" \
  --arg os "macOS ${os_version} (${os_build})" \
  --arg machine_model "${machine_model}" \
  --arg cpu "${cpu_model}" \
  --argjson logical_cpus "${logical_cpus}" \
  --argjson memory_bytes "${memory_bytes}" \
  --arg power "AC high-power mode" \
  --arg protocol_sha256 "${protocol_sha256}" \
  --arg qualification_source_sha256 "${qualification_source_sha256}" \
  --arg qualification_script_sha256 "${qualification_script_sha256}" \
  --arg scenario_manifest_sha256 "${scenario_manifest_sha256}" \
  --arg candidate_lock_sha256 "${candidate_lock_sha256}" \
  --arg reference_lock_sha256 "${reference_lock_sha256}" \
  --arg candidate_source_tree "${candidate_source_tree}" \
  --arg reference_source_tree "${reference_source_tree}" \
  --arg candidate_manifest_sha256 "${candidate_manifest_sha256}" \
  --arg reference_manifest_sha256 "${reference_manifest_sha256}" \
  --arg candidate_original_harness_sha256 "${candidate_original_harness_sha256}" \
  --arg reference_original_harness_sha256 "${reference_original_harness_sha256}" \
  --arg candidate_build_root "${candidate_build_root}" \
  --arg reference_build_root "${reference_build_root}" \
  --arg normal_executable "${normal_executable}" \
  --arg allocation_executable "${allocation_executable}" \
  --arg reference_normal_executable "${reference_normal_executable}" \
  --arg reference_allocation_executable "${reference_allocation_executable}" \
  --arg normal_command "${normal_command}" \
  --arg allocation_command "${allocation_command}" \
  --arg divan_command "${divan_command}" \
  --arg divan_log "${divan_output}" \
  --arg divan_sha256 "${divan_sha256}" \
  --arg cpu_trace "${cpu_trace}" \
  --arg contention_trace "${contention_trace}" \
  --arg heap_profile "${heap_profile}" \
  --arg profile_report "${profile_output}" \
  --argjson compile_seconds "${compile_seconds}" \
  --argjson compile_cycles "${compile_cycles}" \
  --argjson compile_peak_bytes "${compile_peak_bytes}" \
  --argjson binary_size "${binary_size}" \
  --argjson qualification_binary_size "${qualification_binary_size}" \
  --argjson idle_rss_bytes "${idle_rss_bytes}" '
    {
      schema_version: 4,
      suite: "rca-performance-qualification",
      reference_kind: "qualification",
      revision: $revision,
      runner: $runner,
      recorded_at: $recorded_at,
      accepted_regressions: [],
      metadata: {
        compiler: $rustc,
        cargo: $cargo,
        rustc_commit: $rustc_commit,
        os: $os,
        machine_model: $machine_model,
        cpu: $cpu,
        logical_cpus: $logical_cpus,
        memory_bytes: $memory_bytes,
        power: $power,
        cohort: {
          id: $cohort_id,
          recorded_at: $recorded_at,
          runner: $runner,
          compiler: $rustc,
          cargo: $cargo,
          rustc_commit: $rustc_commit,
          os: $os,
          machine_model: $machine_model,
          cpu: $cpu,
          logical_cpus: $logical_cpus,
          memory_bytes: $memory_bytes,
          power: $power,
          protocol_sha256: $protocol_sha256,
          qualification_source_sha256: $qualification_source_sha256,
          qualification_script_sha256: $qualification_script_sha256,
          scenario_manifest_sha256: $scenario_manifest_sha256,
          candidate_lock_sha256: $candidate_lock_sha256,
          reference_lock_sha256: $reference_lock_sha256,
          candidate_source_tree: $candidate_source_tree,
          reference_source_tree: $reference_source_tree,
          candidate_manifest_sha256: $candidate_manifest_sha256,
          reference_manifest_sha256: $reference_manifest_sha256,
          candidate_original_harness_sha256: $candidate_original_harness_sha256,
          reference_original_harness_sha256: $reference_original_harness_sha256
        },
        method: {
          aggregation: "geometric mean of order-stratified paired medians",
          normal_repetitions: 10,
          allocation_repetitions: 6,
          pair: {
            reference_revision: $reference_revision,
            candidate_revision: $revision
          }
        },
        commands: {
          staged_build: "cargo bench --locked --manifest-path <subject>/Cargo.toml --target-dir <subject>/target -p rca-tokio --bench qualification --no-run --message-format=json",
          allocation_build_feature: "qualification-dhat",
          latency: $normal_command,
          allocation: $allocation_command,
          divan: $divan_command
        },
        staged_builds: {
          candidate: {
            root: $candidate_build_root,
            normal_executable: $normal_executable,
            allocation_executable: $allocation_executable
          },
          reference: {
            root: $reference_build_root,
            normal_executable: $reference_normal_executable,
            allocation_executable: $reference_allocation_executable
          }
        }
      },
      supporting_metrics: {
        clean_compile: {
          seconds: $compile_seconds,
          cpu_cycles: $compile_cycles,
          peak_bytes: $compile_peak_bytes
        },
        minimal_consumer_binary_bytes: $binary_size,
        binary_strip: "Cargo release stripping with pinned toolchain libraries",
        qualification_binary_bytes: $qualification_binary_size,
        idle_store_rss_bytes: $idle_rss_bytes,
        divan: {log: $divan_log, sha256: $divan_sha256},
        profiles: {
          cpu_trace: $cpu_trace,
          contention_trace: $contention_trace,
          allocation_profile: $heap_profile,
          report: $profile_report
        }
      },
      rows: $rows[0]
    }
  ' > "${result}"

jq \
  --slurpfile rows "${reference_rows_array}" \
  --arg reference_revision "${reference_revision}" \
  --arg candidate_revision "${revision}" '
    .reference_kind = "paired-reference"
    | .revision = $reference_revision
    | .metadata.method.pair.reference_revision = $reference_revision
    | .metadata.method.pair.candidate_revision = $candidate_revision
    | .supporting_metrics = {paired_candidate_revision: $candidate_revision}
    | .rows = $rows[0]
  ' "${result}" > "${reference_result}"

# Literal backticks are Markdown, not shell substitutions.
# shellcheck disable=SC2016
printf '%s\n' \
  '# RCA performance profile' \
  '' \
  "- Revision: \`${revision}\`" \
  "- Runner: \`${runner}\`" \
  "- Recorded: \`${recorded_at}\`" \
  '' \
  '## Local profiler artefacts' \
  '' \
  "- CPU: \`${cpu_trace}\` (Time Profiler, saturated four-producer commit)" \
  "- Contention: \`${contention_trace}\` (Time Profiler, saturated eight-producer commit)" \
  "- Allocation: \`${heap_profile}\` (DHAT, asynchronous effect)" \
  '' \
  'Raw traces remain under ignored `target/qualification-profiles` because Instruments records host identifiers. Open them locally in Instruments. Do not publish them without redaction.' \
  '' \
  '## Qualification decision' \
  '' \
  "Reference: \`${reference_revision}\`" \
  '' \
  'This run records balanced paired cohort evidence. Acceptance requires strict comparison against its paired reference result.' \
  > "${profile_report}"

stage=evidence-finalisation
check_staged_source "${repo_root}" "${revision}" "${candidate_lock_sha256}" \
  "${qualification_source_sha256}" "${candidate_build_root}"
check_staged_source "${reference_repository}" "${reference_revision}" "${reference_lock_sha256}" \
  "${qualification_source_sha256}" "${reference_build_root}"
if [[ "$(git rev-parse HEAD)" != "${revision}" \
  || -n "$(git status --porcelain=v1 --untracked-files=all)" \
  || "$(shasum -a 256 Cargo.lock | awk '{print $1}')" != "${candidate_lock_sha256}" ]]; then
  echo "candidate checkout changed during qualification" >&2
  exit 1
fi
if [[ -n "${reference_root}" ]] \
  && { [[ "$(git -C "${reference_root}" rev-parse HEAD)" != "${reference_revision}" \
    || -n "$(git -C "${reference_root}" status --porcelain=v1 --untracked-files=all)" ]] \
    || ! cmp -s "${reference_root}/Cargo.lock" docs/performance/baseline-Cargo.lock; }; then
  echo "reference checkout changed during qualification" >&2
  exit 1
fi
if [[ -e "${output}" || -e "${reference_output}" \
  || -e "${profile_output}" || -e "${divan_output}" ]]; then
  echo "qualification evidence path appeared during qualification" >&2
  exit 1
fi
mv -n "${reference_result}" "${reference_output}"
if [[ -e "${reference_result}" ]]; then
  echo "paired reference path appeared during publication" >&2
  exit 1
fi
reference_output_published=1
mv -n "${result}" "${output}"
if [[ -e "${result}" ]]; then
  echo "candidate result path appeared during publication" >&2
  exit 1
fi
output_published=1
mv -n "${divan_log}" "${divan_output}"
if [[ -e "${divan_log}" ]]; then
  echo "Divan result path appeared during publication" >&2
  exit 1
fi
divan_output_published=1
mv -n "${profile_report}" "${profile_output}"
if [[ -e "${profile_report}" ]]; then
  echo "profile result path appeared during publication" >&2
  exit 1
fi
profile_output_published=1
completed=1

echo "performance qualification written: ${output}"
echo "paired reference written: ${reference_output}"
echo "profile report: ${profile_output}"
