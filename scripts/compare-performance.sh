#!/usr/bin/env bash
set -euo pipefail

# Original product identities remain separate from the effective common harness.
check_source_identity() {
  local result=$1 candidate_revision=$2 reference_revision=$3
  local candidate_tree reference_tree candidate_manifest reference_manifest candidate_harness reference_harness
  candidate_tree=$(git rev-parse "${candidate_revision}^{tree}")
  reference_tree=$(git rev-parse "${reference_revision}^{tree}")
  candidate_manifest=$(git show "${candidate_revision}:Cargo.toml" | shasum -a 256 | awk '{print $1}')
  reference_manifest=$(git show "${reference_revision}:Cargo.toml" | shasum -a 256 | awk '{print $1}')
  candidate_harness=$(git show "${candidate_revision}:rca-tokio/benches/qualification.rs" \
    | shasum -a 256 | awk '{print $1}')
  reference_harness=$(git show "${reference_revision}:rca-tokio/benches/qualification.rs" \
    | shasum -a 256 | awk '{print $1}')
  if ! jq -e --arg candidate_tree "${candidate_tree}" --arg reference_tree "${reference_tree}" \
    --arg candidate_manifest "${candidate_manifest}" --arg reference_manifest "${reference_manifest}" \
    --arg candidate_harness "${candidate_harness}" --arg reference_harness "${reference_harness}" '
      .metadata.cohort.candidate_source_tree == $candidate_tree
      and .metadata.cohort.reference_source_tree == $reference_tree
      and .metadata.cohort.candidate_manifest_sha256 == $candidate_manifest
      and .metadata.cohort.reference_manifest_sha256 == $reference_manifest
      and .metadata.cohort.candidate_original_harness_sha256 == $candidate_harness
      and .metadata.cohort.reference_original_harness_sha256 == $reference_harness
    ' "${result}" >/dev/null; then
    echo "source identity does not match the recorded product revisions" >&2
    return 1
  fi
}

# Cover both paired workloads and the candidate's supporting workspace captures.
check_measured_inputs() {
  local candidate_revision=$1 head_revision=$2
  local measured_paths=(
    Cargo.lock Cargo.toml rust-toolchain.toml .cargo dependencies
    rca/Cargo.toml rca/build.rs rca/src
    rca-core/Cargo.toml rca-core/build.rs rca-core/src rca-core/benches
    rca-macros/Cargo.toml rca-macros/build.rs rca-macros/src
    rca-persistence/Cargo.toml rca-persistence/build.rs rca-persistence/src
    rca-test/Cargo.toml rca-test/build.rs rca-test/src rca-test/benches
    rca-tokio/Cargo.toml rca-tokio/build.rs rca-tokio/src rca-tokio/benches
    examples tests/size-probe
  )
  if ! git diff --quiet "${candidate_revision}" "${head_revision}" -- "${measured_paths[@]}" \
    || ! git diff --quiet -- "${measured_paths[@]}" \
    || ! git diff --cached --quiet "${head_revision}" -- "${measured_paths[@]}" \
    || [[ -n "$(git ls-files --others --exclude-standard -- "${measured_paths[@]}")" ]]; then
    echo "measured candidate inputs changed after qualification" >&2
    return 1
  fi
}

# Preserve the independent build receipt, including each executable role.
check_windows_build_receipt() {
  local result=$1 receipt
  receipt=$(dirname "${result}")/source-build.json
  if [[ ! -f "${receipt}" ]] \
    || [[ "$(shasum -a 256 "${receipt}" | awk '{print $1}')" != "$(jq -r '.metadata.source_build_sha256' "${result}")" ]]; then
    echo "Windows source/build receipt is missing or changed" >&2
    return 1
  fi
  if ! jq -e --slurpfile receipt "${receipt}" '
    .metadata as $m | $m.source_build as $s | $m.cohort as $c
    | $s == $receipt[0]
    and ($m.compiler | type == "string" and test("^rustc 1\\.99\\.0 "))
    and ($m.cargo | type == "string" and test("^cargo 1\\.99\\.0 "))
    and ($m.rustc_commit | type == "string" and test("^[0-9a-f]{40}$"))
    and $m.compiler == $c.compiler and $m.compiler == $s.toolchain.rustc
    and $m.cargo == $c.cargo and $m.cargo == $s.toolchain.cargo
    and $m.rustc_commit == $c.rustc_commit and $m.rustc_commit == $s.toolchain.rustc_commit
    and $s.toolchain.target == "x86_64-pc-windows-msvc"
    and $s.harness_sha256 == $c.qualification_source_sha256
    and $s.scenario_manifest_sha256 == $c.scenario_manifest_sha256
    and all(["candidate", "reference"][];
      . as $role
      | $s[$role].revision == $m.method.pair[$role + "_revision"]
      and $s[$role].tree == $c[$role + "_source_tree"]
      and $s[$role].lock_sha256 == $c[$role + "_lock_sha256"]
      and $s[$role].manifest_sha256 == $c[$role + "_manifest_sha256"]
      and $s[$role].original_harness_sha256 == $c[$role + "_original_harness_sha256"])
    and ($m.binaries | keys) == ["candidate_allocation", "candidate_normal", "reference_allocation", "reference_normal"]
    and ($s.binaries | keys) == ($m.binaries | keys)
    and all($m.binaries | keys[];
      . as $role
      | ($m.binaries[$role].sha256 | type == "string" and test("^[0-9a-f]{64}$"))
      and $m.binaries[$role].sha256 == $s.binaries[$role].sha256
      and ($m.binaries[$role].path | type == "string" and length > 0))
  ' "${result}" >/dev/null; then
    echo "Windows source/build receipt does not match measured identities" >&2
    return 1
  fi
}

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <baseline.json> <candidate.json>" >&2
  exit 2
fi

requested_baseline=$1
requested_candidate=$2
regression_limit=1.10

for command in basename dirname git jq mktemp shasum tr; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "required command not found: ${command}" >&2
    exit 1
  fi
done

baseline=$(cd "$(dirname "${requested_baseline}")" && pwd -P)/$(basename "${requested_baseline}")
candidate=$(cd "$(dirname "${requested_candidate}")" && pwd -P)/$(basename "${requested_candidate}")
comparison=${candidate%.json}-comparison.json
if [[ "${baseline}" == "${candidate}" || "${baseline}" == "${comparison}" \
  || "${candidate}" == "${comparison}" \
  || ( -e "${comparison}" && "${comparison}" -ef "${baseline}" ) \
  || ( -e "${comparison}" && "${comparison}" -ef "${candidate}" ) ]]; then
  echo "performance input and output paths must be distinct" >&2
  exit 1
fi

repo_root=$(git rev-parse --show-toplevel)
head_revision=$(git rev-parse HEAD)
revision_pin=${repo_root}/docs/performance/baseline-revision.txt
baseline_lock=${repo_root}/docs/performance/baseline-Cargo.lock
candidate_lock=${repo_root}/Cargo.lock
protocol=${repo_root}/rca-tokio/benches/qualification.rs
qualification_script=${repo_root}/scripts/qualify-performance.sh
windows=false
if [[ -f "${candidate}" ]] && [[ "$(jq -r '.schema_version' "${candidate}")" == "5" ]]; then
  windows=true
  qualification_script=${repo_root}/scripts/capture-windows-performance.py
fi
scenario_manifest=${repo_root}/docs/performance/scenarios.txt
if [[ ! -f "${revision_pin}" || ! -f "${baseline_lock}" \
  || ! -f "${candidate_lock}" || ! -f "${protocol}" \
  || ! -f "${qualification_script}" || ! -f "${scenario_manifest}" ]]; then
  echo "pinned performance inputs are missing" >&2
  exit 1
fi
expected_reference_revision=$(tr -d '\n' < "${revision_pin}")
expected_reference_lock_sha256=$(shasum -a 256 "${baseline_lock}" | awk '{print $1}')
expected_candidate_lock_sha256=$(shasum -a 256 "${candidate_lock}" | awk '{print $1}')
qualification_source_sha256=$(shasum -a 256 "${protocol}" | awk '{print $1}')
expected_qualification_script_sha256=$(shasum -a 256 "${qualification_script}" | awk '{print $1}')
expected_scenario_manifest_sha256=$(shasum -a 256 "${scenario_manifest}" | awk '{print $1}')
protocol_inputs=("${qualification_source_sha256}" "${expected_scenario_manifest_sha256}"
                 "${expected_qualification_script_sha256}")
expected_memory_watchdog_sha256=
if [[ "${windows}" == false ]]; then
  memory_watchdog=${repo_root}/scripts/guard_cargo_memory.py
  if [[ ! -f "${memory_watchdog}" ]]; then
    echo "pinned memory watchdog is missing" >&2
    exit 1
  fi
  expected_memory_watchdog_sha256=$(shasum -a 256 "${memory_watchdog}" | awk '{print $1}')
  protocol_inputs+=("${expected_memory_watchdog_sha256}")
fi
# Windows retains its existing tuple. Old Mac captures remain historical
# evidence; they cannot establish the current memory-guarded qualification.
expected_protocol_sha256=$(printf '%s\n' "${protocol_inputs[@]}" | shasum -a 256 | awk '{print $1}')
expected_scenarios=$(jq -R -s -c 'split("\n") | map(select(length > 0))' \
  "${scenario_manifest}")

for result in "${baseline}" "${candidate}"; do
  if [[ ! -f "${result}" ]]; then
    echo "performance result not found: ${result}" >&2
    exit 1
  fi
  jq -e '
    def valid_memory:
      if $windows then
        (.peak_working_set_bytes | type == "number" and . > 0)
        and (.peak_commit_bytes | type == "number" and . > 0)
        and has("instructions_retired") and .instructions_retired == null
      else
        .peak_resident_bytes > 0 and .peak_memory_footprint_bytes > 0
      end;
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
      and (.candidate | valid_memory)
      and (.reference | valid_memory);
    def valid_allocation:
      same_workload
      and .candidate.allocations > 0
      and .reference.allocations > 0
      and .candidate.allocated_bytes > 0
      and .reference.allocated_bytes > 0
      and .candidate.peak_bytes > 0
      and .reference.peak_bytes > 0;
    (if $windows then
      .schema_version == 5 and .metadata.resource_provider == "win32-process-v1"
    else
      .schema_version == 4 and .metadata.resource_provider == null
    end)
    and .suite == "rca-performance-qualification"
    and (.reference_kind == "paired-reference" or .reference_kind == "qualification")
    and (.revision | type == "string" and length == 40)
    and (.runner | type == "string" and length > 0)
    and (.recorded_at == .metadata.cohort.recorded_at)
    and (.runner == .metadata.cohort.runner)
    and (.metadata.method.aggregation == "geometric mean of order-stratified paired medians")
    and .metadata.method.normal_repetitions == 10
    and .metadata.method.allocation_repetitions == 6
    and (.metadata.method.pair.reference_revision | type == "string" and length == 40)
    and (.metadata.method.pair.candidate_revision | type == "string" and length == 40)
    and (.metadata.cohort.id | type == "string" and length == 64)
    and (.metadata.cohort.protocol_sha256 | type == "string" and length == 64)
    and (.metadata.cohort.qualification_source_sha256 | type == "string" and length == 64)
    and (.metadata.cohort.qualification_script_sha256 | type == "string" and length == 64)
    and (.metadata.cohort.scenario_manifest_sha256 | type == "string" and length == 64)
    and (.metadata.cohort.candidate_lock_sha256 | type == "string" and length == 64)
    and (.metadata.cohort.reference_lock_sha256 | type == "string" and length == 64)
    and (.accepted_regressions | type == "array")
    and (.rows | type == "array" and length == 50)
    and ((.rows | map(.scenario)) == $expected_scenarios)
    and ((.rows | map(.scenario) | unique | length) == (.rows | length))
    and all(.rows[];
      (.scenario | type == "string" and length > 0)
      and expected_samples
      and expected_allocation_samples
      and (.revision == $revision)
      and (.runner == $runner)
      and (.samples > 0)
      and (.operations > 0)
      and (.normal_repetitions == 10)
      and (.allocation_repetitions == 6)
      and (.throughput_per_second > 0)
      and (.latency_ns.p50 > 0)
      and (.latency_ns.p95 > 0)
      and (.latency_ns.p99 > 0)
      and (.cpu_cycles > 0)
      and (.allocations > 0)
      and (.allocation_samples > 0)
      and (.allocation_operations > 0)
      and (.allocated_bytes > 0)
      and (.task_wakes > 0)
      and (.peak_bytes > 0)
      and (if $windows then .instructions_retired == null else .instructions_retired > 0 end)
      and valid_memory
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
      and (. as $row | all(.pairs.normal[];
        valid_normal
        and .candidate.scenario == $row.scenario
        and .candidate.samples == $row.samples
        and .candidate.operations == $row.operations
      ))
      and (. as $row | all(.pairs.allocation[];
        valid_allocation
        and .candidate.scenario == $row.scenario
        and .candidate.samples == $row.allocation_samples
        and .candidate.operations == $row.allocation_operations
      ))
    )
  ' \
    --arg revision "$(jq -r '.revision' "${result}")" \
    --arg runner "$(jq -r '.runner' "${result}")" \
    --argjson windows "${windows}" \
    --argjson expected_scenarios "${expected_scenarios}" \
    "${result}" >/dev/null || {
    echo "invalid performance result schema: ${result}" >&2
    exit 1
  }
  if [[ "${windows}" == true ]]; then
    check_windows_build_receipt "${result}"
  fi
done

if [[ "${windows}" == false ]]; then
  memory_receipt=$(jq -er '.metadata.cohort.memory_watchdog_receipt | select(type == "string")' "${candidate}") || {
    echo "Mac memory watchdog completion receipt is missing" >&2
    exit 1
  }
  if [[ "${memory_receipt}" != /* || ! -f "${memory_receipt}" || -L "${memory_receipt}" ]] \
    || [[ "$(jq -r '.metadata.cohort.memory_watchdog_receipt' "${baseline}")" != "${memory_receipt}" ]] \
    || ! jq -e \
      --arg candidate "${candidate}" --arg reference "${baseline}" \
      --arg candidate_sha256 "$(shasum -a 256 "${candidate}" | awk '{print $1}')" \
      --arg reference_sha256 "$(shasum -a 256 "${baseline}" | awk '{print $1}')" \
      --arg watchdog_sha256 "${expected_memory_watchdog_sha256}" \
      --arg script_sha256 "${expected_qualification_script_sha256}" '
        .entrypoint == "qualify-performance" and .toolchain == "1.99.0"
        and .protection == "polling-watchdog-not-kernel-memory-cap"
        and .started == true and .reason == "command-exited"
        and .returncode == 0 and .command_returncode == 0
        and .cleanup_complete == true and .descendants_complete == true
        and .forced_cleanup == false
        and .memory_limit_bytes == 8589934592 and .host_reserve_bytes == 4294967296
        and .sample_interval_seconds == 0.25
        and (.peak_footprint_bytes | type == "number" and . >= 0 and . < 8589934592 and floor == .)
        and (.peak_process_count | type == "number" and . >= 1 and floor == .)
        and (.elapsed_seconds | type == "number" and . > 0)
        and .memory_watchdog_sha256 == $watchdog_sha256
        and .qualification_script_sha256 == $script_sha256
        and .qualification_outputs == {
          candidate: {path: $candidate, sha256: $candidate_sha256},
          reference: {path: $reference, sha256: $reference_sha256}
        }
      ' "${memory_receipt}" >/dev/null; then
    echo "Mac memory watchdog completion is missing, failed or does not match both outputs" >&2
    exit 1
  fi
fi

candidate_revision=$(jq -r '.revision' "${candidate}")
if ! git cat-file -e "${candidate_revision}^{commit}" 2>/dev/null \
  || ! git merge-base --is-ancestor "${candidate_revision}" "${head_revision}"; then
  echo "candidate evidence revision is not an ancestor of HEAD" >&2
  exit 1
fi
check_measured_inputs "${candidate_revision}" "${head_revision}"
check_source_identity "${candidate}" "${candidate_revision}" "${expected_reference_revision}"
check_source_identity "${baseline}" "${candidate_revision}" "${expected_reference_revision}"

if ! jq -e \
  --arg revision "${candidate_revision}" \
  --arg reference_revision "${expected_reference_revision}" \
  --arg protocol_sha256 "${expected_protocol_sha256}" \
  --arg qualification_source_sha256 "${qualification_source_sha256}" \
  --arg qualification_script_sha256 "${expected_qualification_script_sha256}" \
  --arg memory_watchdog_sha256 "${expected_memory_watchdog_sha256}" \
  --arg scenario_manifest_sha256 "${expected_scenario_manifest_sha256}" \
  --arg candidate_lock_sha256 "${expected_candidate_lock_sha256}" \
  --arg reference_lock_sha256 "${expected_reference_lock_sha256}" '
    .revision == $revision
    and .reference_kind == "qualification"
    and .metadata.method.pair.candidate_revision == $revision
    and .metadata.method.pair.reference_revision == $reference_revision
    and .metadata.cohort.protocol_sha256 == $protocol_sha256
    and .metadata.cohort.qualification_source_sha256 == $qualification_source_sha256
    and .metadata.cohort.qualification_script_sha256 == $qualification_script_sha256
    and ($memory_watchdog_sha256 == "" or .metadata.cohort.memory_watchdog_sha256 == $memory_watchdog_sha256)
    and .metadata.cohort.scenario_manifest_sha256 == $scenario_manifest_sha256
    and .metadata.cohort.candidate_lock_sha256 == $candidate_lock_sha256
    and .metadata.cohort.reference_lock_sha256 == $reference_lock_sha256
  ' "${candidate}" >/dev/null; then
  echo "candidate evidence does not match current revision, protocol, or lockfile" >&2
  exit 1
fi

if ! jq -e \
  --arg reference_revision "${expected_reference_revision}" \
  --arg candidate_revision "${candidate_revision}" \
  --arg protocol_sha256 "${expected_protocol_sha256}" \
  --arg qualification_source_sha256 "${qualification_source_sha256}" \
  --arg qualification_script_sha256 "${expected_qualification_script_sha256}" \
  --arg memory_watchdog_sha256 "${expected_memory_watchdog_sha256}" \
  --arg scenario_manifest_sha256 "${expected_scenario_manifest_sha256}" \
  --arg candidate_lock_sha256 "${expected_candidate_lock_sha256}" \
  --arg reference_lock_sha256 "${expected_reference_lock_sha256}" '
    .revision == $reference_revision
    and .reference_kind == "paired-reference"
    and .metadata.method.pair.reference_revision == $reference_revision
    and .metadata.method.pair.candidate_revision == $candidate_revision
    and .metadata.cohort.protocol_sha256 == $protocol_sha256
    and .metadata.cohort.qualification_source_sha256 == $qualification_source_sha256
    and .metadata.cohort.qualification_script_sha256 == $qualification_script_sha256
    and ($memory_watchdog_sha256 == "" or .metadata.cohort.memory_watchdog_sha256 == $memory_watchdog_sha256)
    and .metadata.cohort.scenario_manifest_sha256 == $scenario_manifest_sha256
    and .metadata.cohort.candidate_lock_sha256 == $candidate_lock_sha256
    and .metadata.cohort.reference_lock_sha256 == $reference_lock_sha256
  ' "${baseline}" >/dev/null; then
  echo "reference evidence does not match pinned revision, protocol, or lockfiles" >&2
  exit 1
fi

temporary=$(mktemp /tmp/rca-performance-comparison.XXXXXX)
trap 'rm -f -- "${temporary}"' EXIT
jq -n \
  --slurpfile baseline "${baseline}" \
  --slurpfile candidate "${candidate}" \
  --argjson windows "${windows}" \
  --argjson limit "${regression_limit}" '
  def median:
    sort as $values
    | ($values | length) as $count
    | if ($count % 2) == 1 then
        $values[$count / 2 | floor]
      else
        (($values[$count / 2 - 1] + $values[$count / 2]) / 2)
      end;
  def stratified_ratio($pairs; metric):
    ([$pairs[] | select(.order == "reference-candidate") | metric] | median) as $ab
    | ([$pairs[] | select(.order == "candidate-reference") | metric] | median) as $ba
    | (($ab * $ba) | sqrt);
  def accepted($candidate; $id):
    $id != null
    and any($candidate.accepted_regressions[]?;
      .id == $id
      and (.review | type == "string" and length > 0)
    );
  $baseline[0] as $base
  | $candidate[0] as $next
  | if $base.metadata.cohort != $next.metadata.cohort
      or $base.metadata.method.pair != $next.metadata.method.pair
      or $base.recorded_at != $next.recorded_at
      or ($windows and ($base.metadata.binaries != $next.metadata.binaries
        or $base.metadata.source_build != $next.metadata.source_build
        or $base.metadata.source_build_sha256 != $next.metadata.source_build_sha256))
    then error("results are not one qualification cohort")
    else .
    end
  | if ($base.rows | length) != ($next.rows | length)
    then error("scenario count mismatch")
    else .
    end
  | [
      $base.rows[] as $before
      | ($next.rows | map(select(.scenario == $before.scenario))) as $matches
      | if ($matches | length) != 1 then
          error("candidate scenario missing or duplicated: \($before.scenario)")
        else
          $matches[0] as $after
          | if $after.samples != $before.samples
              or $after.operations != $before.operations
              or $after.allocation_samples != $before.allocation_samples
              or $after.allocation_operations != $before.allocation_operations
              or $after.pairs != $before.pairs
            then error("candidate and reference workload evidence differs: \($before.scenario)")
            else {
              scenario: $before.scenario,
              median_ratio: stratified_ratio($after.pairs.normal;
                .candidate.latency_ns.p50 / .reference.latency_ns.p50),
              p99_ratio: stratified_ratio($after.pairs.normal;
                .candidate.latency_ns.p99 / .reference.latency_ns.p99),
              throughput_ratio: stratified_ratio($after.pairs.normal;
                .reference.throughput_per_second / .candidate.throughput_per_second),
              allocation_ratio: stratified_ratio($after.pairs.allocation;
                .candidate.allocations / .reference.allocations),
              allocated_bytes_ratio: stratified_ratio($after.pairs.allocation;
                .candidate.allocated_bytes / .reference.allocated_bytes),
              peak_memory_ratio: stratified_ratio($after.pairs.normal;
                if $windows then .candidate.peak_commit_bytes / .reference.peak_commit_bytes
                else .candidate.peak_memory_footprint_bytes / .reference.peak_memory_footprint_bytes end),
              resident_memory_ratio: stratified_ratio($after.pairs.normal;
                if $windows then .candidate.peak_working_set_bytes / .reference.peak_working_set_bytes
                else .candidate.peak_resident_bytes / .reference.peak_resident_bytes end),
              peak_live_heap_ratio: stratified_ratio($after.pairs.allocation;
                .candidate.peak_bytes / .reference.peak_bytes),
              cpu_cycles_ratio: stratified_ratio($after.pairs.normal;
                .candidate.cpu_cycles / .reference.cpu_cycles),
              task_wakes_ratio: stratified_ratio($after.pairs.normal;
                .candidate.task_wakes / .reference.task_wakes),
              rationale_id: ($after.rationale_id // null)
            }
            | .regressed = (
                .median_ratio > $limit
                or .p99_ratio > $limit
                or .throughput_ratio > $limit
                or .allocation_ratio > $limit
                or .allocated_bytes_ratio > $limit
                or .peak_memory_ratio > $limit
                or .resident_memory_ratio > $limit
                or .cpu_cycles_ratio > $limit
                or .task_wakes_ratio > $limit
              )
            | .accepted = (if .regressed then accepted($next; .rationale_id) else false end)
            end
        end
    ] as $comparisons
  | {
      schema_version: $next.schema_version,
      resource_provider: (if $windows then "win32-process-v1" else "darwin-time-lp" end),
      memory_metric: (if $windows then "peak_commit_bytes" else "peak_memory_footprint_bytes" end),
      resident_memory_metric: (if $windows then "peak_working_set_bytes" else "peak_resident_bytes" end),
      regression_limit: $limit,
      cohort_id: $next.metadata.cohort.id,
      aggregation: "geometric mean of order-stratified paired medians",
      baseline: {revision: $base.revision, runner: $base.runner},
      candidate: {revision: $next.revision, runner: $next.runner},
      comparisons: $comparisons,
      unexplained_regressions: [
        $comparisons[] | select(.regressed and (.accepted | not)) | .scenario
      ],
      passed: all($comparisons[]; (.regressed | not) or .accepted)
    }
' > "${temporary}"

mv "${temporary}" "${comparison}"

if ! jq -e '.passed' "${comparison}" >/dev/null; then
  echo "unexplained performance regressions:" >&2
  jq -r '.unexplained_regressions[] | "  \(.)"' "${comparison}" >&2
  echo "comparison: ${comparison}" >&2
  exit 1
fi

echo "performance gate passed: ${comparison}"
