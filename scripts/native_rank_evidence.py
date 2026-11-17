"""Recompute experimental Windows rank bounds from the original process evidence."""

import hashlib
import json
import math
from pathlib import Path
import re
import subprocess


PROTOCOL = "windows-native-rank-experiment-v1"
RESULT = "experimental-rank-result.json"
RULES = Path(__file__).with_name("native_performance_rows.jq")
RULE_BYTES = RULES.read_bytes()
VERIFIER_DIGEST = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def _digest(data):
    return hashlib.sha256(data).hexdigest()


def _same(left, right):
    # JSON Boolean and integer types must not compare equal.
    return json.dumps(left, sort_keys=True, allow_nan=False) == json.dumps(right, sort_keys=True, allow_nan=False)


def row_bounds(jq, scenario, pairs, planned):
    """Use both real order strata and the full comparison's metric directions."""
    missing = {mode: (planned[mode] - len(data)) // 2 for mode, data in pairs.items()}
    query = RULE_BYTES.decode("utf-8") + '''
      . as $row | def bound(limit):
        ($row.missing | map_values([range(.) | limit])) as $unseen
        | native_row_with_unseen($row; true; null; $unseen)
        | . as $result | [.gated_metrics[] | {key: ., value: ($result[.] | if isinfinite then null else . end)}]
        | from_entries;
      def look($mode; $drop):
        ($row.pairs[$mode] | .[:length-$drop]) as $data
        | if ($data|length) < 2 then {monitor:true}
          else ($row | .pairs = ({normal:[], allocation:[]} | .[$mode]=$data))
          | native_prefix_check($mode; $row.missing[$mode]+$drop/2) end;
      [bound(0), bound(infinite), native_regression_limit,
       (["normal","allocation"] | map(. as $m | {key:$m, value:{current:look($m;0),previous:look($m;2)}})
                                | from_entries)]'''
    run = subprocess.run([str(jq), "-c", query],
                         input=json.dumps({"scenario": scenario, "pairs": pairs, "missing": missing}),
                         capture_output=True, text=True, check=True)
    lower, upper, limit, modes = json.loads(run.stdout)
    if (not lower or lower.keys() != upper.keys() or
            any(lower[k] is None or lower[k] < 0 or
                (upper[k] is not None and not lower[k] <= upper[k] < math.inf) for k in lower)):
        raise ValueError(f"invalid computed rank bounds: {scenario}")
    return lower, upper, limit, modes


class _Raw:
    """Read each file once; bind its name and exact bytes into the row digest."""

    def __init__(self, raw):
        if raw.is_symlink() or not raw.is_dir():
            raise ValueError("missing or redirected raw evidence directory")
        self.raw = raw
        self.remaining = {p.name for p in raw.iterdir()}
        self.last_exit = None

    def read(self, name, hashes):
        path = self.raw / name
        if name not in self.remaining or path.is_symlink() or not path.is_file():
            raise ValueError(f"missing, duplicate or redirected raw evidence: {name}")
        self.remaining.remove(name)
        data = path.read_bytes()
        hashes[name] = _digest(data)
        return data


def _seal(hashes):
    return _digest(json.dumps(hashes, sort_keys=True, separators=(",", ":")).encode())


def _process(raw, capture, stem, hashes):
    counters = json.loads(raw.read(stem + ".counters.json", hashes))
    capture.validate_counters(counters)
    if raw.last_exit is not None and counters["CreationTime100ns"] < raw.last_exit:
        raise ValueError(f"process order disagrees with rank evidence: {stem}")
    raw.last_exit = counters["ExitTime100ns"]
    raw.read(stem + ".stderr", hashes)
    return raw.read(stem + ".stdout", hashes), counters


def _pairs(raw, capture, scenario, index, mode, maximum, hashes):
    samples = 10000 if mode == "normal" else capture.allocation_samples(scenario)
    config = "deterministic" if scenario.startswith("deterministic/") else "native"
    result = []
    for n in range(1, maximum + 1):
        suffix = f"{mode}-{index:02d}-{n:02d}"
        filename = f"pair-{suffix}.json"
        if filename not in raw.remaining:
            break
        pair = json.loads(raw.read(filename, hashes))
        order = "reference-candidate" if n % 2 else "candidate-reference"
        expected = {"order": order, "repetition": n}
        for role in order.split("-"):
            stem = role + "-" + suffix
            stdout, counters = _process(raw, capture, stem, hashes)
            value = json.loads(stdout)
            capture.validate_measurement(value, scenario, samples, mode)
            value.update(repetition=n, order=order, configuration=config)
            if mode == "normal":
                value.update(cpu_cycles=counters["QueryProcessCycleTime"], instructions_retired=None,
                             peak_working_set_bytes=counters["PeakWorkingSetSize"],
                             peak_commit_bytes=counters["PeakPagefileUsage"], windows_process_counters=counters)
            else:
                profile = json.loads(raw.read(stem + ".dhat.json", hashes))
                if profile.get("dhatFileVersion") != 2 or not isinstance(profile.get("pps"), list):
                    raise ValueError(f"malformed DHAT profile: {stem}")
            expected[role] = value
        if not _same(pair, expected):
            raise ValueError(f"pair differs from original process measurements: {filename}")
        result.append(pair)
    if len(result) % 2:
        raise ValueError(f"unbalanced {mode} pairs: {scenario}")
    return result


def _check_cohort(capture, cohort, source, source_hash):
    """Resolve repeated source identities against the build, never against each other alone."""
    root = capture.ROOT / "rca-tokio" / "benches"
    hashes = (capture.sha256(root / "qualification.rs"), capture.sha256(root / "qualification/shutdown.rs"),
              capture.sha256(capture.MANIFEST), capture.sha256(Path(capture.__file__)), source_hash)
    expected = dict(zip(("qualification_source_sha256", "qualification_module_sha256", "scenario_manifest_sha256",
                         "qualification_script_sha256", "source_build_sha256"), hashes, strict=True))
    for role in ("candidate", "reference"):
        for field in ("tree", "lock_sha256", "manifest_sha256", "original_harness_sha256"):
            key = "source_tree" if field == "tree" else field
            expected[f"{role}_{key}"] = source[role][field]
    expected.update(compiler=source["toolchain"]["rustc"], cargo=source["toolchain"]["cargo"],
                    rustc_commit=source["toolchain"]["rustc_commit"], system="Windows",
                    configuration=capture.CONFIGURATION, protocol_sha256=_digest(("\n".join(hashes) + "\n").encode()))
    if (not re.fullmatch("[0-9a-f]{64}", cohort["id"])
            or any(cohort.get(key) != value for key, value in expected.items())
            or hashes[:3] != (source["harness_sha256"], source["harness_module_sha256"], source["scenario_manifest_sha256"])):
        raise ValueError("rank cohort source identity differs")


def _inputs(output, capture):
    data = (output / "inputs.json").read_bytes()
    inputs = json.loads(data)
    source_bytes = (output / "source-build.json").read_bytes()
    source = json.loads(source_bytes)
    capture.validate_metadata(source)
    scenarios = capture.load_scenarios()
    meta = inputs["metadata"]
    cohort = meta["cohort"]
    method = meta["method"]
    _check_cohort(capture, cohort, source, _digest(source_bytes))
    if (inputs.get("protocol") != PROTOCOL or inputs["scenarios"] != scenarios
            or inputs.get("rank_rules_sha256") != _digest(RULE_BYTES)
            or inputs.get("rank_verifier_sha256") != VERIFIER_DIGEST
            or not _same(meta["source_build"], source)
            or meta["source_build_sha256"] != _digest(source_bytes)
            or meta["resource_provider"] != capture.PROVIDER or meta["system"] != "Windows"
            or any(meta[field] != cohort[field] for field in ("compiler", "cargo", "rustc_commit"))
            or meta["configuration"] != capture.CONFIGURATION
            or method["aggregation"] != "geometric mean of order-stratified paired medians"
            or not _same(method["normal_repetitions"], 42)
            or not _same(method["allocation_repetitions"], 6)
            or not _same(method["normal_repetition_overrides"], {"deterministic/budget": 10})
            or method["pair"] != {f"{r}_revision": source[r]["revision"] for r in ("candidate", "reference")}):
        raise ValueError("capture method, source or enrolment differs from the experimental protocol")
    binaries = meta["binaries"]
    if (binaries.keys() != source["binaries"].keys()
            or len({v["path"] for v in binaries.values() if v["path"]}) != len(binaries)
            or any(v["sha256"] != source["binaries"][k]["sha256"] for k, v in binaries.items())):
        raise ValueError("binary identities disagree with the source/build receipt")
    return scenarios, _digest(data), _digest(source_bytes)


def build_rank_result(output, capture, jq):
    """Rebuild the decision; do not manufacture a release receipt or point estimates."""
    output = Path(output)
    if any((output / name).exists() for name in ("failure.json", "complete.json", "candidate.json", "reference.json")):
        raise ValueError("failed or canonical output is not experimental rank evidence")
    scenarios, inputs_hash, source_hash = _inputs(output, capture)
    raw = _Raw(output / "raw")
    listed = {}
    list_exits = []
    for key, (_, config, _) in capture.BINARY_ROLES.items():
        raw.last_exit = None  # Supplied binaries can be listed in any order.
        stdout, counters = _process(raw, capture, key + "-list", listed)
        list_exits.append(counters["ExitTime100ns"])
        expected = scenarios if config == "deterministic" else [s for s in scenarios if not s.startswith("deterministic/")]
        if stdout.decode("utf-8").splitlines() != expected:
            raise ValueError(f"binary workload list differs: {key}")
    raw.last_exit = max(list_exits)
    result = {"protocol": PROTOCOL, "qualification": False, "numerical_decision": "pass",
              "inputs_sha256": inputs_hash, "source_build_sha256": source_hash,
              "list_evidence_sha256": _seal(listed), "rows": []}
    proof_path = output / "prefix-rejection.json"
    proof = json.loads(proof_path.read_bytes()) if proof_path.exists() else None
    if proof_path.exists() and (not isinstance(proof, dict) or type(proof.get("row_index")) is not int or
                                not 0 <= proof["row_index"] < len(scenarios) or proof.get("mode") not in ("normal", "allocation")):
        raise ValueError("invalid prefix rejection row or mode")
    for index, scenario in enumerate(scenarios):
        hashes = {}
        planned = {"normal": 10 if scenario == "deterministic/budget" else 42, "allocation": 6}
        pairs = {mode: _pairs(raw, capture, scenario, index, mode, maximum, hashes)
                 for mode, maximum in planned.items()}
        observed = {mode: len(data) for mode, data in pairs.items()}
        rejected = proof is not None and index == proof["row_index"]
        required = ("normal",) if rejected and proof["mode"] == "normal" else planned.keys()
        if any(observed[mode] < 2 for mode in required):
            raise ValueError(f"missing both real run orders: {scenario}")
        lower, upper, limit, modes = row_bounds(jq, scenario, pairs, planned)
        for mode in required:
            if modes[mode]["previous"] != {"monitor": True}:
                raise ValueError(f"observations continued beyond a decisive prefix: {scenario}/{mode}")
        row = {"scenario": scenario, "observed_pairs": observed, "planned_pairs": planned,
               "raw_sha256": _seal(hashes), "lower_bounds": lower, "upper_bounds": upper}
        result["rows"].append(row)
        if rejected:
            failed = modes[proof["mode"]]["current"].get("lower_bounds", {})
            missing = {k: (planned[k] - observed[k]) // 2 for k in planned}
            expected_proof = {"complete": False, "scenario": scenario, "row_index": index,
                              "mode": proof["mode"], "method": "zero-filled order-stratified paired medians",
                              "regression_limit": limit, "planned_pairs": planned,
                              "observed_pairs": observed, "missing_per_order": missing,
                              "lower_bounds": failed, "row_rules_sha256": _digest(RULE_BYTES),
                              "inputs_sha256": inputs_hash,
                              "pair_sha256": {"raw/" + k: h for k, h in hashes.items() if k.startswith("pair-")}}
            if (not failed or not _same(proof, expected_proof)
                    or (proof["mode"] == "allocation" and modes["normal"]["current"] != {"monitor": False})
                    or (proof["mode"] == "normal" and observed["allocation"] != 0)):
                raise ValueError(f"invalid or reversible prefix rejection: {scenario}")
            result.update(numerical_decision="reject", prefix_rejection_sha256=capture.sha256(proof_path))
            break
        if any(v is None or v > limit for v in upper.values()):
            raise ValueError(f"upper bounds do not establish a numerical pass: {scenario}")
    if raw.remaining:
        raise ValueError(f"unexpected or discontinuous raw evidence: {min(raw.remaining)}")
    if capture.sha256(RULES) != _digest(RULE_BYTES) or capture.sha256(Path(__file__)) != VERIFIER_DIGEST:
        raise ValueError("rank verifier or numerical rules changed during replay")
    return result


def verify_rank_result(output, capture, jq):
    """Require exact agreement with the stored experimental certificate."""
    result = build_rank_result(output, capture, jq)
    if not _same(result, json.loads((Path(output) / RESULT).read_bytes())):
        raise ValueError("stored rank decision differs from recomputed evidence")
    return result
