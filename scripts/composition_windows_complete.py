"""Apply unchanged allocation limits and reference-only Windows native limits."""

import hashlib
import json
import math
from pathlib import Path, PureWindowsPath
import statistics

import composition_performance as evidence


POLICY_SHA256 = "9fa4576d8324fd2aa6720f66cb56baba3717cd5f4523bcc1538e8a5ecb3df381"
METHOD = "composition-windows-complete-v2"
RUST100_POLICY_SHA256 = "2797c3ed8965fe31373031e700a6ce8b17e74287fd8de52a5fd9a448fe8af51b"
RUST100_METHOD = "composition-windows-complete-v3"
V4_POLICY_SHA256 = "18a136c60ee24f0d1ef6ce5523158e7dcb53ef76bba9b65051fb8b346a4367ba"
V4_METHOD = "composition-windows-complete-v4"
V5_POLICY_SHA256 = "679e38f61cbbffbfa0b5f6d67435616ac049b7ec5fe5705920929b0ade20edf7"
V5_METHOD = "composition-windows-complete-v5"
V6_POLICY_SHA256 = "a82d2879870f98b57f8e2341d20ec69ad30b881093e1c5afd4feb264e90b3d6a"
V6_METHOD = "composition-windows-complete-v6"


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, allow_nan=False).encode()).hexdigest()


def load_policy(path):
    """Reject changed rules rather than accepting limits chosen after capture."""
    path = Path(path)
    if path.is_symlink() or evidence._sha256(path) not in (POLICY_SHA256, RUST100_POLICY_SHA256, V4_POLICY_SHA256, V5_POLICY_SHA256, V6_POLICY_SHA256):
        raise ValueError("Windows complete policy hash differs from the frozen rules")
    return json.loads(path.read_text(encoding="utf-8"))


def _validate_policy(policy):
    name = {METHOD: "composition-windows-complete-v2.json",
            RUST100_METHOD: "composition-windows-complete-v3.json",
            V4_METHOD: "composition-windows-complete-v4.json",
            V5_METHOD: "composition-windows-complete-v5.json",
            V6_METHOD: "composition-windows-complete-v6.json"}.get(policy.get("method_version"))
    if name is None:
        raise ValueError("Windows complete policy method is unsupported")
    path = Path(__file__).resolve().parents[1] / "docs/performance/composition" / name
    if policy != load_policy(path):
        raise ValueError("Windows complete policy differs from the frozen rules")


def _hash(value):
    return isinstance(value, str) and evidence._HASH.fullmatch(value) is not None


def _timing_observations(budget, timing):
    """Bind all runtime and compile medians to the measured subject."""
    metadata = timing["metadata"]
    evidence._validate_protocol_pair_metadata(metadata, timing.get("method_version"))
    if (not evidence.is_windows_protocol(timing)
            or metadata.get("method_version") != timing.get("method_version")
            or timing.get("protocol_sha256") != evidence._protocol_sha256(timing)
            or timing.get("runner") != evidence.WINDOWS_RUNNER
            or timing.get("toolchain") != evidence.expected_toolchain(timing)
            or timing.get("mode") != metadata["mode"]
            or timing.get("role") != metadata["subject_role"]
            or timing.get("revision") != metadata["subject_revision"]):
        raise ValueError("Windows timing report does not match its subject metadata")
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
    return values


def _observations(budget, timing, raw):
    """Bind native measurements to the paired subject, source and workload set."""
    return _timing_observations(budget, timing) | _product_observations(budget, timing["metadata"], raw)


def _product_observations(budget, metadata, raw):
    """Validate the complete allocation, zero-host and PE workload without timing data."""
    values = {}
    if (raw.get("schema") != "composition-windows-absolute-raw-v1"
            or raw.get("subject_revision") != metadata["subject_revision"]
            or raw.get("allocation_manifest_sha256") != evidence.ALLOCATION_MANIFEST_SHA256
            or raw.get("source_receipt_sha256") != metadata["workload_provenance"]["subject"]["receipt_sha256"]
            or raw.get("root_lock_sha256") != metadata["subject_lock_sha256"]):
        raise ValueError("Windows absolute observation has different source provenance")
    for field in ("source_receipt_sha256", "root_lock_sha256", "allocation_manifest_sha256"):
        if not _hash(raw.get(field)) or raw.get("input_hashes_after", {}).get(field) != raw[field]:
            raise ValueError("Windows absolute input hash changed")

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


def _reference_rows(policy, budget_rows, values):
    """Freeze limits from reference values only; allocation limits are absolute."""
    rows = []
    for row in budget_rows:
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
    return rows


def seal_reference(policy, budget_path, timing_report, raw_observation):
    """Derive new native limits only from the pinned A/A subject arm."""
    _validate_policy(policy)
    if timing_report.get("protocol_sha256") != policy["paired_protocol_sha256"]:
        raise ValueError("Windows complete policy belongs to another paired protocol")
    if (timing_report.get("mode") != "reference-reference-aa"
            or timing_report.get("revision") != policy["reference_revision"]
            or timing_report.get("role") != "reference-b"):
        raise ValueError("Windows limits require reference-only A/A observations")
    budget = evidence.validate_budget_document(Path(budget_path))
    rows = _reference_rows(policy, budget["rows"], _observations(budget, timing_report, raw_observation))
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
    policy_hash = {METHOD: POLICY_SHA256, RUST100_METHOD: RUST100_POLICY_SHA256,
                   V4_METHOD: V4_POLICY_SHA256, V5_METHOD: V5_POLICY_SHA256,
                   V6_METHOD: V6_POLICY_SHA256}[policy["method_version"]]
    return {"schema": "composition-windows-reference-limits-v1", "policy_sha256": policy_hash,
            "reference_revision": policy["reference_revision"],
            "capture_sha256": timing_report["metadata"]["capture_sha256"],
            "reference_timing": reference_timing, "reference_raw": raw_observation, "rows": rows}


def _validated_budget(policy, budget_path, timing_report, baseline):
    """Recompute the reference limits and check that the subject belongs to this capture."""
    expected = seal_reference(policy, budget_path, baseline["reference_timing"], baseline["reference_raw"])
    if baseline != expected:
        raise ValueError("Windows reference baseline differs from its source observations")
    if timing_report["metadata"]["capture_sha256"] != baseline["capture_sha256"]:
        raise ValueError("Windows baseline belongs to another capture")
    reference_host = baseline["reference_timing"]["metadata"]
    if any(timing_report["metadata"].get(field) != reference_host.get(field)
           for field in ("platform", "target", "os_version", "os_build")):
        raise ValueError("Windows baseline belongs to another host")
    if timing_report.get("protocol_sha256") != policy["paired_protocol_sha256"]:
        raise ValueError("Windows complete policy belongs to another paired protocol")
    return evidence.validate_budget_document(Path(budget_path))


def _result(row, values):
    value = values[row["scenario"], row["measured_statistic"]]
    return {**row, "measured_value": value,
            "passed": evidence._passes(value, row["absolute_budget"], row["direction"])}


def _experimental_product_input(policy, mode, metadata, raw):
    """Bind the parsed observations to independently authenticated capture inputs."""
    if not isinstance(metadata, dict) or not isinstance(raw, dict):
        raise ValueError("missing experimental product evidence")
    evidence._validate_protocol_pair_metadata(metadata, evidence.WINDOWS_V6_METHOD)
    if (metadata.get("method_version") != evidence.WINDOWS_V6_METHOD or metadata.get("mode") != mode
            or metadata.get("protocol_sha256") != policy["paired_protocol_sha256"]
            or metadata.get("runner") != policy["runner"]
            or metadata.get("toolchain") != evidence.expected_toolchain(metadata)
            or metadata.get("budget_sha256") != policy["budget_sha256"]
            or metadata.get("allocation_manifest_sha256") != policy["allocation_manifest_sha256"]
            or not all(_hash(metadata.get(k)) for k in ("capture_sha256", "subject_lock_sha256", "control_lock_sha256"))
            or not _hash(metadata.get("workload_provenance", {}).get("subject", {}).get("receipt_sha256"))
            or metadata.get("raw_content_sha256") != _digest(raw)
            or not _hash(raw.get("allocation_binary_sha256")) or raw.get("binary_execution_qualified") is not False):
        raise ValueError("experimental product evidence differs from capture inputs")
    tools = metadata.get("tools", {})
    strip_tools = {"python", "lief_wheel", "lief_init", "lief_native"}
    required_tools = strip_tools | {"cargo", "rustc", "rustdoc", "compiler_telemetry", "objcopy"}
    if not isinstance(tools, dict) or set(tools) != required_tools or any(
        not isinstance(tool, dict) or not isinstance(tool.get("path"), str) or not tool["path"]
        or not _hash(tool.get("sha256")) for tool in tools.values()
    ):
        raise ValueError("experimental product tools are not hash-bound")
    for row in raw.get("binaries", {}).values():
        removal = row.get("debug_removal", {})
        stripped = removal.get("stripped", {})
        observed_tools = removal.get("tools", {})
        if (row.get("strip_tool_sha256") != tools["objcopy"]["sha256"]
                or stripped.get("sha256") != row.get("sha256") or stripped.get("size_bytes") != row.get("bytes")
                or set(observed_tools) != strip_tools or any(
                    any(observed_tools[k].get(field) != tools[k][field] for field in ("path", "sha256"))
                    for k in strip_tools)):
            raise ValueError("experimental PE tool or stripped-product identity differs")


def experimental_product_gates(policy, budget_path, aa_capture_metadata, aa_raw,
                               candidate_capture_metadata, candidate_raw):
    """Evaluate only the 19 product gates in both Windows roles, never timing or release qualification.

    Each metadata record carries its authenticated inputs.json tools and the independently
    verified raw_content_sha256 (_digest of the parsed observation). Callers authenticate files.
    """
    _validate_policy(policy)
    if policy["method_version"] != V6_METHOD:
        raise ValueError("experimental product gates require the Windows v6 policy")
    budget = evidence.validate_budget_document(Path(budget_path))
    aa, cand = "reference-reference-aa", "candidate-reference"
    _experimental_product_input(policy, aa, aa_capture_metadata, aa_raw)
    aa_values = _product_observations(budget, aa_capture_metadata, aa_raw)
    product_rows = [row for row in budget["rows"] if row["scenario"].startswith(("allocation/", "binary/"))]
    limits = _reference_rows(policy, product_rows, aa_values)
    _experimental_product_input(policy, cand, candidate_capture_metadata, candidate_raw)
    shared = ("capture_sha256", "platform", "target", "os_version", "os_build", "control_lock_sha256", "tools")
    if (any(aa_capture_metadata[k] != candidate_capture_metadata[k] for k in shared)
            or aa_capture_metadata["subject_lock_sha256"] != aa_capture_metadata["control_lock_sha256"]):
        raise ValueError("experimental product modes differ in capture, host, tools or reference lock")
    cand_values = _product_observations(budget, candidate_capture_metadata, candidate_raw)
    rows = {mode: [_result(row, values) for row in limits] for mode, values in ((aa, aa_values), (cand, cand_values))}
    failures = {mode: [row for row in results if not row["passed"]] for mode, results in rows.items()}
    bindings = {mode: {"subject_revision": meta["subject_revision"], "source_receipt_sha256": raw["source_receipt_sha256"],
                       "root_lock_sha256": raw["root_lock_sha256"], "raw_content_sha256": meta["raw_content_sha256"],
                       "allocation_binary_sha256": raw["allocation_binary_sha256"], "allocation_workloads": len(raw["allocations"]),
                       "zero_tests": raw["zero_tests"], "tools_sha256": _digest(meta["tools"])}
                for mode, meta, raw in ((aa, aa_capture_metadata, aa_raw), (cand, candidate_capture_metadata, candidate_raw))}
    return {"schema": "experimental-windows-product-gates-v1", "timing_evaluated": False,
            "policy_sha256": V6_POLICY_SHA256, "capture_sha256": aa_capture_metadata["capture_sha256"],
            "reference_limits": limits, "rows": rows, "failed_budgets": failures, "evidence": bindings,
            "passed": not any(failures.values())}


def _retained(root, name):
    """Open only the expected capture-relative file, never a published path from JSON."""
    path = root / name
    if not path.is_relative_to(root) or path.resolve() != path or not path.is_file():
        raise ValueError(f"missing or redirected experimental evidence: {name}")
    return path


def _verify_retained_pe(raw, label, row):
    """Recheck the original, LLVM intermediate, stripped output and the strip receipt offline."""
    import composition_windows_absolute as absolute
    import windows_strip
    orig = f"binaries/{label}-original.exe"
    final = f"binaries/{label}-stripped.exe"
    original, stripped = _retained(raw, orig), _retained(raw, final)
    intermediate = _retained(raw, f"binaries/{label}-llvm-stripped.exe")
    receipt = _retained(raw, f"binaries/{label}-stripped.strip.json")
    strip_log = _retained(raw, f"strip-{label}.txt")
    debug_log = _retained(raw, f"debug-strip-{label}.txt")
    removal = row["debug_removal"]
    if (row["original_path"] != orig or row["path"] != final or evidence._sha256(original) != row["before_sha256"]
            or row["strip_log_sha256"] != evidence._sha256(strip_log) or evidence._sha256(stripped) != row["sha256"]
            or absolute._pe_identity(stripped) != row["bytes"] or stripped.stat().st_size > original.stat().st_size
            or json.loads(_retained(raw, f"{label}.json").read_bytes()) != row):
        raise ValueError(f"retained PE differs from its sealed observation: {label}")
    for field, path in (("intermediate", intermediate), ("stripped", stripped), ("receipt", receipt),
                        ("log", debug_log), ("helper", Path(windows_strip.__file__))):
        fact = removal[field]
        if (PureWindowsPath(fact["path"]).name != path.name or fact["sha256"] != evidence._sha256(path)
                or fact["size_bytes"] != path.stat().st_size):
            raise ValueError(f"retained PE strip {field} differs: {label}")
    actual = json.loads(receipt.read_bytes())
    command = removal["command"]
    tool = removal["tools"]
    expected_command = [tool["python"]["path"], "-I", "-S", removal["helper"]["path"], "--wheel",
                        tool["lief_wheel"]["path"], "--site", str(PureWindowsPath(tool["lief_init"]["path"]).parent.parent),
                        "--input", removal["intermediate"]["path"], "--output", removal["stripped"]["path"]]
    if (actual != {k: v for k, v in removal.items() if k not in ("receipt", "log", "helper", "command")}
            or list(map(PureWindowsPath, command)) != list(map(PureWindowsPath, expected_command))
            or actual["lief_version"] != windows_strip.LIEF_VERSION or actual["configuration"] != windows_strip.CONFIGURATION
            or actual["verification"] != windows_strip.validate_transform(intermediate.read_bytes(), stripped.read_bytes())):
        raise ValueError(f"retained PE debug transformation differs: {label}")


def _verified_cargo_products(log, root, expected, *, allocating=None):
    """Match fresh executable records to staged subject sources without opening removed targets."""
    allowed = {
        "binary-fixture-build": ("absolute-windows-binary", "absolute-windows-binary-fixtures"),
        "binary-tokio-build": ("absolute-windows-binary", "absolute-windows-binary-root"),
        "binary-app-test-build": ("absolute-windows-binary", "absolute-windows-binary-root"),
        "zero-build": ("absolute-windows-zero",),
        "allocation-build": ("subject", "absolute-windows-allocation"),
    }[log.stem]
    records = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines() if line.startswith("{")]
    for name, kind, source in expected:
        matching = [r for r in records if r.get("reason") == "compiler-artifact" and r.get("executable")
                    and r["target"]["name"] == name and kind in r["target"]["kind"]]
        if len(matching) != 1:
            raise ValueError(f"missing or duplicate retained Cargo product: {name}")
        row = matching[0]
        executable = PureWindowsPath(row["executable"])
        if (row.get("fresh") is not False or PureWindowsPath(row["target"]["src_path"]) != root / source
                or not any(executable.is_relative_to(root.parent / name) for name in allowed)
                or ".." in executable.parts
                or (allocating is not None and ("allocation-measurement" in row["features"]) != allocating)):
            raise ValueError(f"foreign or cached Cargo product: {name}")


def _read_experimental_mode(directory, mode, sha):
    import composition_windows_absolute as absolute
    arm = directory / mode
    raw = arm / "absolute-windows-raw"
    content = _retained(raw, "observation.json").read_bytes()
    if hashlib.sha256(content).hexdigest() != sha:
        raise ValueError(f"{mode} sealed observation bytes changed")
    obs = json.loads(content)
    meta = json.loads(_retained(arm, "metadata.json").read_bytes())
    inputs = json.loads(_retained(arm, "inputs.json").read_bytes())
    if inputs["mode"] != mode or meta["mode"] != mode:
        raise ValueError("experimental product mode differs")
    for role in ("subject", "control"):
        identity = inputs[role]
        path = _retained(arm, f"sources/{role}.json")
        source = json.loads(path.read_bytes())
        if (source["revision"] != identity["revision"] or source["tree"] != identity["tree"]
                or meta[f"{role}_revision"] != identity["revision"]
                or identity["hashes"]["Cargo.lock"] != source["effective"]["Cargo.lock"]
                or meta[f"{role}_lock_sha256"] != source["effective"]["Cargo.lock"]
                or evidence._sha256(path) != meta["workload_provenance"][role]["receipt_sha256"]):
            raise ValueError(f"{mode} {role} source receipt or lock differs")
        if role == "subject":
            root = PureWindowsPath(source["root"])
    aggregate = _retained(raw, "allocations.txt")
    if absolute.allocation_observations(aggregate) != obs["allocations"]:
        raise ValueError("retained allocation totals differ")
    for line in aggregate.read_text(encoding="utf-8").splitlines():
        name = line.split()[0].removeprefix("scenario=")
        if _retained(raw, f"allocation/{name}.txt").read_text(encoding="utf-8").splitlines() != [line]:
            raise ValueError(f"retained allocation transcript differs: {name}")
    for label, test in absolute.ZERO_TESTS.items():
        if absolute._zero_test(_retained(raw, f"zero-{label}.txt"), test) != obs["zero_tests"][label]:
            raise ValueError(f"retained zero transcript differs: {label}")
    for log, products in (
        ("binary-fixture-build", [(f"rca-compile-{n}", "bin", f"benches/compile/{n}/src/main.rs")
                                  for n in absolute.BINARY_NAMES[:4]]),
        ("binary-tokio-build", [("rca-example-composition-search", "bin", "examples/composition-search/src/main.rs")]),
        ("binary-app-test-build", [("app", "test", "examples/composition-search/tests/app.rs")]),
        ("allocation-build", [("composition", "bench", "rca-core/benches/composition.rs")]),
        ("zero-build", [("composition_zero", "test", "rca-core/tests/composition_qualification/main.rs")]),
    ):
        _verified_cargo_products(_retained(raw, f"{log}.jsonl"), root, products,
                                 allocating=True if log in ("allocation-build", "zero-build") else None)
    for label in absolute.BINARY_NAMES:
        _verify_retained_pe(raw, label, obs["binaries"][label])
    return {**meta, "tools": inputs["tools"], "raw_content_sha256": _digest(obs)}, obs


def load_experimental_products(directory, policy, budget_path, expected_digests):
    """Replay retained products against independently sealed SHA-256s of both observation files."""
    modes = ("reference-reference-aa", "candidate-reference")
    if not isinstance(expected_digests, dict) or set(expected_digests) != set(modes) or not all(
        _hash(expected_digests[m]) for m in modes
    ):
        raise ValueError("both experimental modes need sealed observation digests")
    directory = Path(directory).resolve(strict=True)
    try:
        aa, candidate = (_read_experimental_mode(directory, mode, expected_digests[mode]) for mode in modes)
        result = experimental_product_gates(policy, budget_path, *aa, *candidate)
    except (KeyError, TypeError, AttributeError, IndexError) as error:
        raise ValueError("incomplete or malformed retained experimental products") from error
    return {**result, "observation_file_sha256": dict(expected_digests)}


def timing_failures(policy, budget_path, timing_report, baseline):
    """Reject an already-failed timing gate; an empty list cannot certify the candidate."""
    budget = _validated_budget(policy, budget_path, timing_report, baseline)
    values = _timing_observations(budget, timing_report)
    rows = (_result(row, values) for row in baseline["rows"]
            if row["scenario"].startswith(("runtime/", "compile/")))
    return [row for row in rows if not row["passed"]]


def evaluate(policy, budget_path, timing_report, raw_observation, baseline):
    """Apply sealed limits without changing them or certifying macOS results."""
    budget = _validated_budget(policy, budget_path, timing_report, baseline)
    values = _observations(budget, timing_report, raw_observation)
    rows = [_result(row, values) for row in baseline["rows"]]
    failed = [row for row in rows if not row["passed"]]
    return {"schema_version": 1, "method_version": policy["method_version"],
            "policy_sha256": baseline["policy_sha256"],
            "qualification_scope": "windows-complete", "mac_absolute_qualified": False,
            "mode": timing_report["mode"], "subject_revision": timing_report["revision"],
            "reference_only": timing_report["mode"] == "reference-reference-aa",
            "baseline_content_sha256": _digest(baseline), "raw_content_sha256": _digest(raw_observation),
            "rows": rows, "failed_budgets": failed, "passed": not failed}
