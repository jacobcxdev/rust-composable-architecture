"""Apply unchanged allocation limits and reference-only Windows native limits."""

import hashlib
import json
import math
from pathlib import Path
import statistics

import composition_performance as evidence


POLICY_SHA256 = "23f7fef0e20dc4d6c5f5bf93a74443a0b6f0588f117626f6daf99ae3fd2c6cab"
METHOD = "composition-windows-complete-v1"


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, allow_nan=False).encode()).hexdigest()


def load_policy(path):
    """Reject changed rules rather than accepting limits chosen after capture."""
    path = Path(path)
    if path.is_symlink() or evidence._sha256(path) != POLICY_SHA256:
        raise ValueError("Windows complete policy hash differs from the frozen rules")
    return json.loads(path.read_text(encoding="utf-8"))


def _validate_policy(policy):
    path = Path(__file__).resolve().parents[1] / "docs/performance/composition/composition-windows-complete-v1.json"
    if policy != load_policy(path):
        raise ValueError("Windows complete policy differs from the frozen rules")


def _hash(value):
    return isinstance(value, str) and evidence._HASH.fullmatch(value) is not None


def _observations(budget, timing, raw):
    """Bind native measurements to the paired subject, source and workload set."""
    metadata = timing["metadata"]
    evidence._validate_protocol_pair_metadata(metadata, evidence.WINDOWS_METHOD)
    if (timing.get("method_version") != evidence.WINDOWS_METHOD
            or timing.get("protocol_sha256") != evidence.WINDOWS_PROTOCOL_SHA256
            or timing.get("runner") != evidence.WINDOWS_RUNNER
            or timing.get("toolchain") != evidence.WINDOWS_TOOLCHAIN
            or timing.get("mode") != metadata["mode"]
            or timing.get("role") != metadata["subject_role"]
            or timing.get("revision") != metadata["subject_revision"]):
        raise ValueError("Windows timing report does not match its subject metadata")
    if (raw.get("schema") != "composition-windows-absolute-raw-v1"
            or raw.get("subject_revision") != timing["revision"]
            or raw.get("allocation_manifest_sha256") != evidence.ALLOCATION_MANIFEST_SHA256
            or raw.get("source_receipt_sha256") != metadata["workload_provenance"]["subject"]["receipt_sha256"]
            or raw.get("root_lock_sha256") != metadata["subject_lock_sha256"]):
        raise ValueError("Windows absolute observation has different source provenance")
    for field in ("source_receipt_sha256", "root_lock_sha256", "allocation_manifest_sha256"):
        if not _hash(raw.get(field)) or raw.get("input_hashes_after", {}).get(field) != raw[field]:
            raise ValueError("Windows absolute input hash changed")

    expected_timing = {(row["scenario"], row["measured_statistic"]): row for row in budget["rows"]
                       if row["scenario"].startswith(("runtime/", "compile/"))}
    values = {}
    for row in timing["rows"]:
        key = row["scenario"], row["measured_statistic"]
        if key not in expected_timing or key in values or row.get("units") != expected_timing[key]["units"]:
            raise ValueError("Windows timing row set or units differ")
        samples = row["process_values"]
        if len(samples) != 20 or [item["block"] for item in samples] != list(range(1, 21)):
            raise ValueError("Windows timing requires twenty ordered observations")
        if any(type(item["value"]) not in (int, float) or not math.isfinite(item["value"])
               or item["value"] <= 0 for item in samples):
            raise ValueError("Windows timing values must be finite and positive")
        values[key] = statistics.median(item["value"] for item in samples)
    if set(values) != set(expected_timing):
        raise ValueError("Windows timing row set is incomplete")

    manifest = evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    allocations = raw.get("allocations", {})
    if set(allocations) != set(manifest):
        raise ValueError("Windows allocation evidence requires all forty-seven workloads")
    fields = {"samples", "logical_operations", "allocations", "allocated_bytes", "retained_bytes", "peak_live_bytes"}
    for name, workload in manifest.items():
        row = allocations[name]
        if (set(row) != fields or any(type(value) is not int or value < 0 for value in row.values())
                or any(row[field] != workload[field] for field in ("samples", "logical_operations"))):
            raise ValueError("Windows allocation workload counts differ")
    zero_names = {
        "direct_scope": "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
        "combine": "composition::combine::tests::combine_all_none_widths_do_not_allocate",
    }
    zero = raw.get("zero_tests", {})
    if set(zero) != set(zero_names) or any(
        zero[name].get("test") != test or type(zero[name].get("passed")) is not int
        or zero[name]["passed"] != 1 or zero[name].get("failed") != 0
        or not _hash(zero[name].get("stdout_sha256")) for name, test in zero_names.items()
    ):
        raise ValueError("Windows absolute evidence needs both exact successful zero tests")
    selected = {}
    for row in budget["rows"]:
        if row["scenario"].startswith("allocation/"):
            parts = row["scenario"].split("/")
            value = 0 if len(parts) == 2 else allocations[parts[1]][parts[2]]
            selected[row["scenario"]] = value
            values[row["scenario"], row["measured_statistic"]] = value
    if raw.get("selected") != selected:
        raise ValueError("Windows selected allocations differ from complete observations")

    binaries = raw.get("binaries", {})
    expected_binaries = {row["scenario"].split("/", 1)[1] for row in budget["rows"]
                         if row["scenario"].startswith("binary/")}
    if set(binaries) != expected_binaries:
        raise ValueError("Windows binary evidence needs all six products")
    for name, row in binaries.items():
        if (row.get("format") != "PE-x86_64" or type(row.get("bytes")) is not int or row["bytes"] <= 0
                or any(not _hash(row.get(field)) for field in
                       ("sha256", "before_sha256", "strip_log_sha256", "strip_tool_sha256"))):
            raise ValueError("Windows binary evidence is not a hash-bound stripped PE")
        values[f"binary/{name}", "stripped_file_size"] = row["bytes"]
    return values


def seal_reference(policy, budget_path, timing_report, raw_observation):
    """Derive new native limits only from the pinned A/A subject arm."""
    _validate_policy(policy)
    if (timing_report.get("mode") != "reference-reference-aa"
            or timing_report.get("revision") != evidence.REFERENCE_REVISION
            or timing_report.get("role") != "reference-b"):
        raise ValueError("Windows limits require reference-only A/A observations")
    budget = evidence.validate_budget_document(Path(budget_path))
    values = _observations(budget, timing_report, raw_observation)
    rows = []
    for row in budget["rows"]:
        value = values[row["scenario"], row["measured_statistic"]]
        direction = evidence._direction(row)
        if row["scenario"].startswith("allocation/"):
            limit = row["absolute_budget"]
        elif row["scenario"].startswith("binary/"):
            # Integer arithmetic avoids rounding an exact byte boundary up twice.
            limit = (value * 11 + 9) // 10
        elif direction == "minimum":
            limit = value / policy["regression_limit"]
        else:
            limit = value * policy["regression_limit"]
        rows.append({"scenario": row["scenario"], "measured_statistic": row["measured_statistic"],
                     "units": row["units"], "direction": direction, "reference_value": value,
                     "absolute_budget": limit})
    # Paired validation binds and replays the raw transcripts. Keep their numeric
    # observations here, not paths that change when staging is published.
    reference_timing = {key: timing_report[key] for key in (
        "method_version", "protocol_sha256", "runner", "toolchain", "mode", "role", "revision", "metadata",
    )}
    reference_timing["rows"] = [
        {"scenario": row["scenario"], "measured_statistic": row["measured_statistic"], "units": row["units"],
         "process_values": [{"block": sample["block"], "value": sample["value"]}
                            for sample in row["process_values"]]}
        for row in timing_report["rows"]
    ]
    return {"schema": "composition-windows-reference-limits-v1", "policy_sha256": POLICY_SHA256,
            "reference_revision": evidence.REFERENCE_REVISION,
            "capture_sha256": timing_report["metadata"]["capture_sha256"],
            "reference_timing": reference_timing, "reference_raw": raw_observation, "rows": rows}


def evaluate(policy, budget_path, timing_report, raw_observation, baseline):
    """Apply sealed limits without changing them or certifying macOS results."""
    expected = seal_reference(policy, budget_path, baseline["reference_timing"], baseline["reference_raw"])
    if baseline != expected:
        raise ValueError("Windows reference baseline differs from its source observations")
    if timing_report["metadata"]["capture_sha256"] != baseline["capture_sha256"]:
        raise ValueError("Windows baseline belongs to another capture")
    reference_host = baseline["reference_timing"]["metadata"]
    if any(timing_report["metadata"].get(field) != reference_host.get(field)
           for field in ("platform", "target", "os_version", "os_build")):
        raise ValueError("Windows baseline belongs to another host")
    budget = evidence.validate_budget_document(Path(budget_path))
    values = _observations(budget, timing_report, raw_observation)
    rows = []
    for row in baseline["rows"]:
        value = values[row["scenario"], row["measured_statistic"]]
        rows.append({**row, "measured_value": value,
                     "passed": evidence._passes(value, row["absolute_budget"], row["direction"])})
    failed = [row for row in rows if not row["passed"]]
    return {"schema_version": 1, "method_version": METHOD, "policy_sha256": POLICY_SHA256,
            "qualification_scope": "windows-complete", "mac_absolute_qualified": False,
            "mode": timing_report["mode"], "subject_revision": timing_report["revision"],
            "reference_only": timing_report["mode"] == "reference-reference-aa",
            "baseline_content_sha256": _digest(baseline), "raw_content_sha256": _digest(raw_observation),
            "rows": rows, "failed_budgets": failed, "passed": not failed}
