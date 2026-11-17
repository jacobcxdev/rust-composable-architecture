# Shared numerical gates only. Release acceptance also needs full cohort validation.
def native_regression_limit: 1.10;

def median:
  sort as $values
  | ($values | length) as $count
  | if ($count % 2) == 1 then
      $values[$count / 2 | floor]
    else
      (($values[$count / 2 - 1] + $values[$count / 2]) / 2)
    end;
def stratified_ratio($pairs; metric):
  def order_median($order):
    ([$pairs.observed[] | select(.order == $order) | metric] + $pairs.unseen) | median;
  order_median("reference-candidate") as $ab
  | order_median("candidate-reference") as $ba
  | (($ab * $ba) | sqrt);
def gated_metrics:
  ["median_ratio", "p99_ratio", "throughput_ratio", "allocation_ratio",
   "allocated_bytes_ratio", "peak_memory_ratio", "resident_memory_ratio",
   "cpu_cycles_ratio", "task_wakes_ratio"];
def scenario_gated_metrics:
  if (.scenario == "shutdown/abortive/live/1"
      or .scenario == "shutdown/abortive/live/8"
      or .scenario == "shutdown/abortive/burst/8"
      or .scenario == "shutdown/abortive/burst/64")
  then ["peak_memory_ratio", "resident_memory_ratio"]
  elif .scenario == "shutdown/abortive"
  then gated_metrics - ["peak_memory_ratio", "resident_memory_ratio"]
  else gated_metrics
  end;
def accepted($candidate; $comparison):
  $comparison.rationale_id != null
  and any($candidate.accepted_regressions[]?;
    .id == $comparison.rationale_id
    and .scenario == $comparison.scenario
    and (.review | type == "string" and test("\\S"))
    and (.metrics | if type == "array" then
      length > 0 and ((. - gated_metrics) | length == 0)
      else false end)
    and (($comparison.regressed_metrics - .metrics) | length == 0)
  );

# Bounds substitute limits for the missing ratios in each order. The completed
# comparator never pads either order.
def native_row_with_unseen($after; $windows; $candidate; $unseen):
  ($after | .pairs |= with_entries(. as $mode | .value =
    {observed: $mode.value, unseen: $unseen[$mode.key]})) as $after
  | {
    scenario: $after.scenario,
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
  | .gated_metrics = scenario_gated_metrics
  | .diagnostic_metrics = (gated_metrics - .gated_metrics + ["peak_live_heap_ratio"])
  | .regressed_metrics = [
      .gated_metrics[] as $metric | select(.[$metric] > native_regression_limit) | $metric
    ]
  | .regressed = (.regressed_metrics | length > 0)
  | .accepted = (if .regressed then accepted($candidate; .) else false end);

def native_row_comparison($after; $windows; $candidate):
  native_row_with_unseen($after; $windows; $candidate; {normal: [], allocation: []});

# The collector supplies validated balanced pairs in the active mode only.
# The inactive mode contributes zero to both checks; it is monitored separately.
def native_prefix_check($mode; $missing_per_order):
  . as $row
  | def bound(limit):
      ({normal: [0], allocation: [0]} | .[$mode] = [range($missing_per_order) | limit]) as $unseen
      | native_row_with_unseen($row; true; null; $unseen);
  bound(0) as $lower
  | if $lower.regressed and ($lower.accepted | not) then
      {limit: native_regression_limit,
       lower_bounds: ([$lower.regressed_metrics[] | {key: ., value: $lower[.]}] | from_entries)}
    else {monitor: (bound(infinite) | .regressed and (.accepted | not))} end;
