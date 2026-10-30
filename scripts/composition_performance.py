#!/usr/bin/env python3
"""Validate and compare RCA composition performance evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import pathlib
import re
import statistics
import subprocess
import sys
from typing import Any, Sequence


REFERENCE_REVISION = "b9eb6e732d2b0cf6ce3cd7255a5958781587af6c"
BUDGET_SHA256 = (
    "6f71abeab57bc421bd11a53d30699ad9981d7af097932ee0e01ef477e9a38d69"
)
BASELINE_RUNNER = "mac15-9-m3-max-16c-48gb-macos26-ac-high"
BINARY_BASELINE_RUNNER = "mac15-9-m3-max-16c-48gb-macos27-binary-correction"
RUNNER = "mac15-9-m3-max-16c-48gb-macos27plus-ac-high"
MINIMUM_MACOS_MAJOR = 27
TOOLCHAIN = (
    "rustc 1.98.0 (88d9e12ae 2026-08-18); "
    "cargo 1.98.0 (797e8a9bc 2026-08-05)"
)
REGRESSION_LIMIT = 1.10
EXPECTED_DIVAN_ROWS = 153
EXPECTED_ALLOCATIONS = 47
ALLOCATION_MANIFEST_SHA256 = (
    "95a9e2ee87deaa3046d4a51893efe164282d51aef6a3b3eec61e9f1ca0b3bf32"
)
PROTOCOL_SHA256 = (
    "956d2264039c6f71412309dfd7c6765cb667cc0bb965ebdee41b807741d92332"
)
DEFAULT_ALLOCATION_MANIFEST = pathlib.Path(__file__).with_name(
    "composition-allocation-workloads.tsv"
)

_TREE_ROW = re.compile(r"^(?P<prefix>(?:(?:│  |   ))*)(?:├─|╰─) (?P<rest>.*)$")
_TIMING_ROW = re.compile(
    r"^(?P<name>.*?)\s{2,}"
    r"(?P<fast>[0-9]+(?:\.[0-9]+)?) (?P<fast_unit>ps|ns|[µμ]s|us|ms|s)\s+│\s*"
    r"(?P<slow>[0-9]+(?:\.[0-9]+)?) (?P<slow_unit>ps|ns|[µμ]s|us|ms|s)\s+│\s*"
    r"(?P<median>[0-9]+(?:\.[0-9]+)?) (?P<median_unit>ps|ns|[µμ]s|us|ms|s)\s+│"
)
_ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
_HASH = re.compile(r"^[0-9a-f]{64}$")
_REVISION = re.compile(r"^[0-9a-f]{40}$")
_TIMESTAMP = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")


def validate_composable_stream_work(value: Any, expected_engine: str) -> list[dict[str, Any]]:
    """Validate both finite traces without claiming buffering or performance parity."""
    if expected_engine not in ("composable", "rca"):
        raise ValueError("finite stream engine is invalid")
    if type(value) is not list or len(value) != 2:
        raise ValueError("finite stream requires both cases")
    for row, (case, trace) in zip(value, (("empty", []), ("ordered", [3, 1, 4, 2]))):
        expected = dict(schema="composable-finite-stream-v1", engine=expected_engine,
                        case=case, expected=trace, observed=trace,
                        terminal_reduced=True, producer_exhausted=True,
                        stream_drops=1, state_extracted=True, host_stopped=True)
        if type(row) is not dict or row.keys() != expected.keys():
            raise ValueError("finite stream fields are invalid")
        for field, required in expected.items():
            actual = row[field]
            if type(actual) is not type(required) or actual != required:
                raise ValueError(f"finite stream mismatch: {field}")
            if isinstance(required, list) and any(type(item) is not int for item in actual):
                raise ValueError(f"finite stream trace type is invalid: {field}")
    return value


def capture_composable_stream_work(command: Sequence[str], expected_engine: str) -> list[dict[str, Any]]:
    """Admit both JSON lines only after successful bounded fixture execution."""
    result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=60)
    return validate_composable_stream_work(
        [json.loads(line) for line in result.stdout.splitlines()], expected_engine,
    )


def validate_composable_ready_future_work(value: Any, expected_engine: str) -> dict[str, Any]:
    """Validate finite ready-future work without equating task-join guarantees."""
    if expected_engine not in ("composable", "rca"):
        raise ValueError("ready future engine is invalid")
    expected = dict(schema="composable-ready-future-v1", engine=expected_engine,
                    operations_expected=64, operations_attempted=64,
                    operations_completed=64, results=64, checksum=6496,
                    future_polls=64, future_drops=64, errors=0,
                    state_extracted=True, host_stopped=True,
                    completion="causal-finish" if expected_engine == "rca" else "result-acknowledgement")
    if type(value) is not dict or value.keys() != expected.keys():
        raise ValueError("ready future fields are invalid")
    for field, required in expected.items():
        if type(value[field]) is not type(required) or value[field] != required:
            raise ValueError(f"ready future mismatch: {field}")
    return value


def capture_composable_ready_future_work(command: Sequence[str], expected_engine: str) -> dict[str, Any]:
    """Admit one ledger only after successful bounded fixture execution."""
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("ready future JSON has duplicate fields")
            result[key] = value
        return result

    result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=60)
    return validate_composable_ready_future_work(
        json.loads(result.stdout, object_pairs_hook=unique_object), expected_engine)


def validate_composable_counter_work(value: Any) -> dict[str, Any]:
    """Validate the fixed completed-work ledger from a successful fixture process."""
    expected = {
        "schema": "composable-counter-work-v1",
        "warmup_expected": 20,
        "warmup_attempted": 20,
        "warmup_completed": 20,
        "operations_expected": 10000,
        "operations_attempted": 10000,
        "operations_completed": 10000,
        "initial": 0,
        "final": 10020,
        "errors": 0,
        "state_extracted": True,
        "host_stopped": True,
    }
    if not isinstance(value, dict) or value.keys() != expected.keys() | {"engine"}:
        raise ValueError("composable counter ledger fields are invalid")
    if type(value["engine"]) is not str or value["engine"] not in ("composable", "rca"):
        raise ValueError("composable counter engine is invalid")
    for field, required in expected.items():
        if type(value[field]) is not type(required) or value[field] != required:
            raise ValueError(f"composable counter ledger mismatch: {field}")
    return value


def capture_composable_counter_work(command: Sequence[str]) -> dict[str, Any]:
    """Admit one ledger only after its fixture process exits successfully."""
    result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=60)
    return validate_composable_counter_work(json.loads(result.stdout))


def validate_composable_counter_timing(value: Any) -> dict[str, Any]:
    """Validate raw timings without conferring host or statistical qualification."""
    fields = {"schema", "clock", "unit", "work", "loop_ns", "samples_ns"}
    if not isinstance(value, dict) or value.keys() != fields:
        raise ValueError("composable counter timing fields are invalid")
    for field, required in (("schema", "composable-counter-timing-v1"),
                            ("clock", "std::time::Instant"), ("unit", "ns")):
        if type(value[field]) is not str or value[field] != required:
            raise ValueError(f"composable counter timing mismatch: {field}")
    validate_composable_counter_work(value["work"])
    elapsed = value["loop_ns"]
    samples = value["samples_ns"]
    if type(elapsed) is not int or not 0 < elapsed < 2**128:
        raise ValueError("composable counter loop duration is invalid")
    if type(samples) is not list or len(samples) != 10000:
        raise ValueError("composable counter sample count is invalid")
    if any(type(sample) is not int or not 0 <= sample < 2**128 for sample in samples):
        raise ValueError("composable counter latency is invalid")
    if sum(samples) > elapsed:
        raise ValueError("composable counter loop does not cover its samples")
    return value


def capture_composable_counter_timing(
    command: Sequence[str], expected_engine: str,
) -> dict[str, Any]:
    """Admit timings only after successful process exit and matching engine identity."""
    if expected_engine not in ("composable", "rca"):
        raise ValueError("expected counter engine is invalid")
    result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=60)
    value = validate_composable_counter_timing(json.loads(result.stdout))
    if value["work"]["engine"] != expected_engine:
        raise ValueError("counter measurement came from the wrong engine")
    return value


def validate_composable_counter_lifecycle(value: Any) -> dict[str, Any]:
    """Validate one raw lifecycle measurement without statistical qualification."""
    expected = {
        "schema": "composable-counter-lifecycle-v1",
        "clock": "std::time::Instant",
        "unit": "ns",
        "operations_expected": 1,
        "operations_attempted": 1,
        "operations_completed": 1,
        "initial": 0,
        "final": 1,
        "errors": 0,
        "state_extracted": True,
        "host_stopped": True,
    }
    if not isinstance(value, dict) or value.keys() != expected.keys() | {"engine", "elapsed_ns"}:
        raise ValueError("composable counter lifecycle fields are invalid")
    if type(value["engine"]) is not str or value["engine"] not in ("composable", "rca"):
        raise ValueError("composable counter engine is invalid")
    for field, required in expected.items():
        if type(value[field]) is not type(required) or value[field] != required:
            raise ValueError(f"composable counter lifecycle mismatch: {field}")
    elapsed = value["elapsed_ns"]
    if type(elapsed) is not int or not 0 < elapsed < 2**128:
        raise ValueError("composable counter lifecycle duration is invalid")
    return value


def capture_composable_counter_lifecycle(
    command: Sequence[str], expected_engine: str,
) -> dict[str, Any]:
    """Admit lifecycle data only after successful exit and matching engine identity."""
    if expected_engine not in ("composable", "rca"):
        raise ValueError("expected counter engine is invalid")
    result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=60)
    value = validate_composable_counter_lifecycle(json.loads(result.stdout))
    if value["engine"] != expected_engine:
        raise ValueError("counter measurement came from the wrong engine")
    return value


COUNTER_ALLOCATION_PROTOCOL_SHA256 = "a8b7b60fc273b65c36a6e11e825514fc767f0f4819e5d554d70f8a20a831316b"
_COUNTER_ALLOCATION_DOCUMENT_SHA256 = "26994909097173278d556e31d2ab1b675be4ca0b463c98ba708fcb3795ee37db"
READY_FUTURE_ALLOCATION_PROTOCOL_SHA256 = "fc2180814e1f594f889b554880be6c701f5b68798f5c5971933a0969e19f2e2a"
_READY_FUTURE_ALLOCATION_DOCUMENT_SHA256 = "a33d09edc1d6086581f6932902abc031ab49dd77c981f658e3b8c66272fdd01b"


def validate_composable_ready_future_allocations(value: Any, expected_engine: str) -> dict:
    """Admit native ready-future costs without asserting equivalent task joins."""
    fields = {"schema", "engine", "instrument", "work", "snapshots", "phases"}
    if type(value) is not dict or value.keys() != fields:
        raise ValueError("ready future allocation fields are invalid")
    validate_composable_ready_future_work(value["work"], expected_engine)
    for field, required in (("schema", "composable-ready-future-allocations-v1"),
                            ("instrument", "dhat-0.3.3"), ("engine", expected_engine)):
        if type(value[field]) is not str or value[field] != required:
            raise ValueError(f"ready future allocation mismatch: {field}")
    _validate_allocation_windows(value, {
        "construction": ("A", "B", 1), "initial": ("B", "C", 20),
        "checked": ("C", "D", 44), "teardown": ("D", "E", 1),
        "setup": ("A", "C", 1)})
    return value


def decode_ready_future_allocations(text: str, expected_engine: str) -> dict:
    """Reject duplicate fields before validating allocation observations."""
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("ready future allocation JSON has duplicate fields")
            result[key] = value
        return result
    return validate_composable_ready_future_allocations(
        json.loads(text, object_pairs_hook=unique_object), expected_engine)


def capture_composable_ready_future_allocations(command: Sequence[str], expected_engine: str) -> dict:
    """Require one valid document and successful bounded process completion."""
    result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=60)
    return decode_ready_future_allocations(result.stdout, expected_engine)


def validate_composable_counter_allocations(value: Any, expected_engine: str) -> dict:
    """Admit exact process-wide allocation snapshots and their declared deltas."""
    fields = {"schema", "engine", "instrument", "work", "snapshots", "phases"}
    if type(value) is not dict or value.keys() != fields:
        raise ValueError("counter allocation fields are invalid")
    if type(expected_engine) is not str or expected_engine not in ("composable", "rca"):
        raise ValueError("expected counter allocation engine is invalid")
    for field, required in (("schema", "composable-counter-allocations-v1"),
                            ("instrument", "dhat-0.3.3"), ("engine", expected_engine)):
        if type(value[field]) is not str or value[field] != required:
            raise ValueError(f"counter allocation mismatch: {field}")
    validate_composable_counter_work(value["work"])
    if value["work"]["engine"] != expected_engine:
        raise ValueError("counter allocation work came from the wrong engine")
    _validate_allocation_windows(value, {
        "construction": ("A", "B", 1), "warmup": ("B", "C", 20),
        "checked": ("C", "D", 10000), "teardown": ("D", "E", 1),
        "setup": ("A", "C", 1)})
    return value


def _validate_allocation_windows(value: dict, windows: dict) -> None:
    """Check five cumulative/live snapshots and exact workload-specific deltas."""
    snapshots = value["snapshots"]
    counters = {"allocated_blocks", "allocated_bytes", "live_blocks", "live_bytes"}
    if type(snapshots) is not dict or snapshots.keys() != set("ABCDE"):
        raise ValueError("allocation snapshots are invalid")
    previous = None
    for name in "ABCDE":
        snapshot = snapshots[name]
        if type(snapshot) is not dict or snapshot.keys() != counters:
            raise ValueError("allocation snapshot fields are invalid")
        if any(type(count) is not int or not 0 <= count < 2**64 for count in snapshot.values()):
            raise ValueError("allocation count is invalid")
        for suffix in ("blocks", "bytes"):
            total = snapshot[f"allocated_{suffix}"]
            if snapshot[f"live_{suffix}"] > total:
                raise ValueError("live allocation count exceeds cumulative total")
            if previous is not None and total < previous[f"allocated_{suffix}"]:
                raise ValueError("cumulative allocation count decreased")
        previous = snapshot
    phases = value["phases"]
    if type(phases) is not dict or phases.keys() != windows.keys():
        raise ValueError("allocation phases are invalid")
    for name, (start, end, operations) in windows.items():
        phase = phases[name]
        required = {"operations": operations,
                    **{field: snapshots[end][field] - snapshots[start][field]
                       for field in ("allocated_blocks", "allocated_bytes")}}
        if (type(phase) is not dict or phase.keys() != required.keys()
                or any(type(phase[field]) is not int or phase[field] != count
                       for field, count in required.items())):
            raise ValueError("allocation phase disagrees with snapshots or work")


def decode_counter_allocations(text: str, expected_engine: str) -> dict:
    """Reject duplicate JSON keys before strict allocation admission."""
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("counter allocation JSON has duplicate fields")
            result[key] = value
        return result
    return validate_composable_counter_allocations(
        json.loads(text, object_pairs_hook=unique_object), expected_engine)


def load_counter_allocation_protocol(path: pathlib.Path) -> dict:
    """Admit the allocation protocol's frozen bytes and canonical document."""
    return _load_counter_protocol(path, COUNTER_ALLOCATION_PROTOCOL_SHA256,
                                  _COUNTER_ALLOCATION_DOCUMENT_SHA256)


def load_ready_future_allocation_protocol(path: pathlib.Path) -> dict:
    """Admit the separately frozen ready-future allocation protocol."""
    return _load_counter_protocol(path, READY_FUTURE_ALLOCATION_PROTOCOL_SHA256,
                                  _READY_FUTURE_ALLOCATION_DOCUMENT_SHA256)


def ready_future_allocation_measurement_plan(protocol: dict) -> list[dict]:
    """Expand the ready-future order only after exact protocol validation."""
    _validate_counter_document(protocol, _READY_FUTURE_ALLOCATION_DOCUMENT_SHA256)
    return _allocation_measurement_plan(protocol)


def counter_allocation_measurement_plan(protocol: dict) -> list[dict]:
    """Expand three forward/reverse cycles without data-dependent ordering."""
    _validate_counter_document(protocol, _COUNTER_ALLOCATION_DOCUMENT_SHA256)
    return _allocation_measurement_plan(protocol)


def _allocation_measurement_plan(protocol: dict) -> list[dict]:
    plan = []
    for cycle in range(1, protocol["cycles"] + 1):
        for order in protocol["orders"]:
            engines = protocol["engine_order"]
            if order["order"] == "reverse":
                engines = list(reversed(engines))
            for engine in engines:
                plan.append(dict(sequence=len(plan) + 1, cycle=cycle,
                                 **order, engine=engine))
    return plan


def evaluate_counter_allocations(rows: list[dict], protocol: dict) -> dict:
    """Retain raw costs and observed differences without statistical inference."""
    plan = counter_allocation_measurement_plan(protocol)
    return _evaluate_allocations(rows, protocol, plan, validate_composable_counter_allocations,
                                 "composable-counter-allocation-analysis-v1",
                                 COUNTER_ALLOCATION_PROTOCOL_SHA256)


def evaluate_ready_future_allocations(rows: list[dict], protocol: dict) -> dict:
    """Retain costs and completion differences without claiming equivalent joins."""
    plan = ready_future_allocation_measurement_plan(protocol)
    result = _evaluate_allocations(rows, protocol, plan, validate_composable_ready_future_allocations,
                                   "composable-ready-future-allocation-analysis-v1",
                                   READY_FUTURE_ALLOCATION_PROTOCOL_SHA256)
    result["completion"] = dict(protocol["completion"])
    result["qualified_comparison"] = False
    result["interpretation"] = (
        "observed allocation costs including fixture coordination; completion guarantees differ; "
        "no steady-state, statistical or superiority claim")
    return result


def _evaluate_allocations(rows, protocol, plan, validate, schema, protocol_sha256):
    if type(rows) is not list or len(rows) != len(plan):
        raise ValueError("counter allocation observations are incomplete")
    for row, expected in zip(rows, plan):
        if (type(row) is not dict or row.keys() != expected.keys() | {"raw"}
                or any(type(row[key]) is not type(value) or row[key] != value
                       for key, value in expected.items())):
            raise ValueError("counter allocation observation order or identity is invalid")
        validate(row["raw"], expected["engine"])
    engines = {}
    for engine in protocol["engine_order"]:
        samples = [row["raw"] for row in rows if row["engine"] == engine]
        phases = {}
        for phase in protocol["phases"]:
            operations = samples[0]["phases"][phase]["operations"]
            metrics = {}
            for metric in protocol["metrics"]:
                values = [sample["phases"][phase][metric] for sample in samples]
                metrics[metric] = dict(values=values, min=min(values), max=max(values),
                                       per_operation=[value / operations for value in values])
            phases[phase] = dict(operations=operations, **metrics)
        engines[engine] = dict(samples=len(samples), phases=phases)
    comparisons = {}
    by_identity = {(row["cycle"], row["order"], row["engine"]): row["raw"] for row in rows}
    for name, control_engine in (("candidate-composable", "composable"),):
        comparisons[name] = []
        for row in rows:
            if row["engine"] != "rca":
                continue
            control = by_identity[row["cycle"], row["order"], control_engine]
            phases = {}
            for phase in protocol["phases"]:
                phases[phase] = {}
                operations = row["raw"]["phases"][phase]["operations"]
                for metric in protocol["metrics"]:
                    before = control["phases"][phase][metric]
                    after = row["raw"]["phases"][phase][metric]
                    status = ("observed-zeros" if before == after == 0 else
                              "zero-control" if before == 0 else
                              "zero-subject" if after == 0 else "positive")
                    phases[phase][metric] = dict(
                        control=before, subject=after, delta=after - before,
                        delta_per_operation=(after - before) / operations,
                        ratio=after / before if before else None, status=status)
            comparisons[name].append(dict(cycle=row["cycle"], order=row["order"],
                                           role=row["role"], phases=phases))
    return dict(schema=schema,
                protocol_sha256=protocol_sha256,
                provenance_qualified=False, observations=rows, engines=engines,
                comparisons=comparisons,
                interpretation="observed allocation costs only; no statistical or superiority claim")


COUNTER_PROTOCOL_SHA256 = "0fd70efb7a438041c7032ff28ee673fa14256cacaea84ed80eb1186273610d7e"
_COUNTER_DOCUMENT_SHA256 = "b8ec974ba744d938216ba1974e0738d95559ae8a1938ae79b48b79284ecdb7d6"
COUNTER_LIFECYCLE_PROTOCOL_SHA256 = "6ccee60c1c572b2a48d899ce07ceaa395179e2a3fecc51131eb955e1bee7a0c8"
_COUNTER_LIFECYCLE_DOCUMENT_SHA256 = "c887683a8eb2f706e38304ee081c5d91dc9996bdd7a6b64a88d462cb814042ed"


def _validate_counter_protocol(protocol: dict) -> None:
    _validate_counter_document(protocol, _COUNTER_DOCUMENT_SHA256)


def _validate_counter_document(protocol: dict, document_sha256: str) -> None:
    try:
        encoded = json.dumps(protocol, sort_keys=True, separators=(",", ":"),
                             allow_nan=False).encode()
    except (TypeError, ValueError) as error:
        raise ValueError("counter protocol document is invalid") from error
    if hashlib.sha256(encoded).hexdigest() != document_sha256:
        raise ValueError("counter protocol document is not the frozen protocol")


MINIMAL_SIZE_PROTOCOL_SHA256 = "49099b439eb6955be4510fd2c32b9956123dacea8609df02514914caf645da54"
_MINIMAL_SIZE_DOCUMENT_SHA256 = "c13e4367f846da8b29198e336293b30f9e890ffac1bd434ca933a63f864ff594"


MINIMAL_COMPILE_PROTOCOL_SHA256 = "a07d4a2c9402617d79faa585b6700eae9949706ffd578025d654266c0ab4dfa4"
_MINIMAL_COMPILE_DOCUMENT_SHA256 = "9d9fe942d23d2cc8cab0d9b787c727ed40d17c58587f95644197ca7e3a510c46"


def load_minimal_compile_protocol(path: pathlib.Path) -> dict:
    return _load_counter_protocol(path, MINIMAL_COMPILE_PROTOCOL_SHA256,
                                  _MINIMAL_COMPILE_DOCUMENT_SHA256)


def minimal_compile_plan(protocol: dict) -> list[dict]:
    _validate_counter_document(protocol, _MINIMAL_COMPILE_DOCUMENT_SHA256)
    return [dict(block=block, workload=workload, engine=engine)
            for block, order in enumerate(protocol['orders'], 1)
            for workload in (protocol['workloads'] if block % 2 else protocol['workloads'][::-1])
            for engine in order]


def evaluate_minimal_compile(rows: list[dict], protocol: dict) -> dict:
    """Admit a complete descriptive compile diagnostic, never a regression gate."""
    plan = minimal_compile_plan(protocol)
    fields = {'real_seconds', 'user_seconds', 'system_seconds', 'maximum_resident_set_bytes'}
    if not isinstance(rows, list) or len(rows) != len(plan):
        raise ValueError('minimal compile requires the complete schedule')
    for row, item in zip(rows, plan):
        if (not isinstance(row, dict) or set(row) != {*item, 'counters'}
                or any(type(row[key]) is not type(value) or row[key] != value for key, value in item.items())):
            raise ValueError('minimal compile observation identity differs')
        counters = row['counters']
        if not isinstance(counters, dict) or set(counters) != fields:
            raise ValueError('minimal compile counters differ')
        for key, value in counters.items():
            if (type(value) not in (int, float) or not math.isfinite(value) or value < 0
                    or (key == 'maximum_resident_set_bytes' and type(value) is not int)):
                raise ValueError('minimal compile counter is invalid')

    def values(engine, workload, metric):
        counters = [row['counters'] for row in rows if row['engine'] == engine and row['workload'] == workload]
        return [(c['user_seconds'] + c['system_seconds']) if metric == 'cpu_seconds' else c[metric]
                for c in counters]

    metrics = sorted(fields | {'cpu_seconds'})
    summaries = {engine: {workload: {} for workload in protocol['workloads']} for engine in protocol['engines']}
    ratios = {engine: {workload: {} for workload in protocol['workloads']} for engine in protocol['engines'] if engine != 'rca'}
    for engine in protocol['engines']:
        for workload in protocol['workloads']:
            for metric in metrics:
                samples = values(engine, workload, metric)
                summaries[engine][workload][metric] = dict(median=statistics.median(samples),
                                                          minimum=min(samples), maximum=max(samples))
                if engine != 'rca':
                    ratios[engine][workload][metric] = [subject / control if subject > 0 and control > 0 else None
                        for subject, control in zip(values('rca', workload, metric), samples)]
    return dict(observations=rows, summaries=summaries, ratios=ratios,
                timing_qualified=False, qualified_comparison=False, regression_pass=False,
                rss_label='Maximum reported process RSS across the accounted command and reaped descendants')


def load_minimal_size_protocol(path: pathlib.Path) -> dict:
    return _load_counter_protocol(path, MINIMAL_SIZE_PROTOCOL_SHA256,
                                  _MINIMAL_SIZE_DOCUMENT_SHA256)


def evaluate_minimal_size(rows: list[dict], protocol: dict) -> dict:
    """Validate four size observations without interpreting build or run time."""
    _validate_counter_document(protocol, _MINIMAL_SIZE_DOCUMENT_SHA256)
    if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
        raise ValueError("minimal size observations must be a list of objects")
    expected = [(engine, build) for engine in protocol["engines"] for build in protocol["builds"]]
    if [(row.get("engine"), row.get("build")) for row in rows] != expected:
        raise ValueError("minimal size observations are missing, reordered or duplicated")
    fields = {"engine", "build", "original_bytes", "stripped_bytes", "original_sha256",
              "stripped_sha256", "target", "format", "original_stdout", "stripped_stdout",
              "original_exit", "stripped_exit", "strip_exit"}
    for row in rows:
        if set(row) != fields:
            raise ValueError("minimal size observation fields differ")
        if row["target"] != protocol["target"] or row["format"] != protocol["format"]:
            raise ValueError("minimal size executable target differs")
        for phase in ("original", "stripped"):
            if type(row[f"{phase}_bytes"]) is not int or row[f"{phase}_bytes"] <= 0:
                raise ValueError("minimal size byte count is invalid")
            if not isinstance(row[f"{phase}_sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", row[f"{phase}_sha256"]):
                raise ValueError("minimal size binary hash is invalid")
            if row[f"{phase}_stdout"] != protocol["expected_stdout"]:
                raise ValueError("minimal size application did not complete expected work")
        if any(type(row[key]) is not int or row[key] != 0
               for key in ("original_exit", "stripped_exit", "strip_exit")):
            raise ValueError("minimal size process failed")
    return {"schema": "composable-minimal-size-analysis-v1",
            "protocol_sha256": MINIMAL_SIZE_PROTOCOL_SHA256,
            "observations": rows, "timing_qualified": False, "qualified_comparison": False}


def load_counter_protocol(path: pathlib.Path) -> dict:
    """Admit only the frozen bytes and document, including in-memory consumers."""
    return _load_counter_protocol(path, COUNTER_PROTOCOL_SHA256, _COUNTER_DOCUMENT_SHA256)


def load_counter_lifecycle_protocol(path: pathlib.Path) -> dict:
    """Admit only the separately frozen lifecycle protocol bytes and document."""
    return _load_counter_protocol(path, COUNTER_LIFECYCLE_PROTOCOL_SHA256,
                                  _COUNTER_LIFECYCLE_DOCUMENT_SHA256)


def _load_counter_protocol(path: pathlib.Path, file_sha256: str, document_sha256: str) -> dict:
    try:
        raw = path.read_bytes()
        if hashlib.sha256(raw).hexdigest() != file_sha256:
            raise ValueError("counter protocol file hash is invalid")
        protocol = json.loads(raw)
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read counter protocol: {error}") from error
    _validate_counter_document(protocol, document_sha256)
    return protocol


def counter_measurement_plan(protocol: dict) -> list[dict]:
    """Return the fixed balanced process order without changing the older protocol."""
    _validate_counter_protocol(protocol)
    return _counter_measurement_plan(protocol)


def counter_lifecycle_measurement_plan(protocol: dict) -> list[dict]:
    """Return the separately seeded, balanced lifecycle process order."""
    _validate_counter_document(protocol, _COUNTER_LIFECYCLE_DOCUMENT_SHA256)
    return _counter_measurement_plan(protocol)


def _counter_measurement_plan(protocol: dict) -> list[dict]:
    labelled = [
        (hashlib.sha256(f"{protocol['seed']}:{order}:{index}".encode()).hexdigest(), order)
        for order in ("control-subject", "subject-control")
        for index in range(1, protocol["pairs_per_order"] + 1)
    ]
    orders = [order for _, order in sorted(labelled)]
    return [dict(cohort, blocks=[{"block": index, "order": order}
                                for index, order in enumerate(orders, 1)])
            for cohort in protocol["cohorts"]]


def _counter_metrics(raw: dict, protocol: dict) -> dict:
    samples = sorted(raw["samples_ns"])
    return dict(zip(protocol["metrics"], [
        *(samples[rank - 1] for rank in protocol["percentile_ranks"]),
        raw["loop_ns"] / protocol["operations"],
    ]))


def _counter_ratio_summary(rows: list[dict], metric: str, protocol: dict) -> dict:
    ratios = [row["subject"][metric] / row["control"][metric]
              if row["subject"][metric] > 0 and row["control"][metric] > 0 else None
              for row in rows]
    strata = {order: [ratio for row, ratio in zip(rows, ratios) if row["order"] == order]
              for order in ("control-subject", "subject-control")}
    result = {
        "available": None not in ratios, "ratios": ratios, "strata": strata,
        "estimate": None, "lower": None, "upper": None, "direction": "unavailable",
        "order_factor": None, "drift_factors": None,
        "order_suitable": False, "drift_suitable": False,
    }
    if not result["available"]:
        result["reason"] = "zero-resolution percentile; no pseudocount or discarded block"
        return result
    medians = [statistics.median(values) for values in strata.values()]
    lower = math.sqrt(math.prod(min(values) for values in strata.values()))
    upper = math.sqrt(math.prod(max(values) for values in strata.values()))
    drift = {}
    for order, values in strata.items():
        halves = (statistics.median(values[:6]), statistics.median(values[6:]))
        drift[order] = max(halves) / min(halves)
    order_factor = max(medians) / min(medians)
    result.update(
        estimate=math.sqrt(math.prod(medians)), lower=lower, upper=upper,
        direction="lower-cost" if upper < 1 else "higher-cost" if lower > 1 else "unresolved",
        order_factor=order_factor, drift_factors=drift,
        order_suitable=order_factor <= protocol["max_order_factor"],
        drift_suitable=all(factor <= protocol["max_drift_factor"] for factor in drift.values()),
    )
    return result


def evaluate_counter_cohorts(cohorts: dict, protocol: dict) -> dict:
    """Analyse complete raw ledgers; numerical checks never authenticate provenance."""
    plan = counter_measurement_plan(protocol)
    return _evaluate_counter_cohorts(cohorts, protocol, plan, lifecycle=False)


def evaluate_counter_lifecycle_cohorts(cohorts: dict, protocol: dict) -> dict:
    """Analyse complete lifecycle ledgers without authenticating their provenance."""
    plan = counter_lifecycle_measurement_plan(protocol)
    return _evaluate_counter_cohorts(cohorts, protocol, plan, lifecycle=True)


def _evaluate_counter_cohorts(cohorts: dict, protocol: dict, plan: list[dict],
                             *, lifecycle: bool) -> dict:
    if type(cohorts) is not dict or cohorts.keys() != {item["name"] for item in plan}:
        raise ValueError("counter cohort set is incomplete or unexpected")
    admitted = {}
    # Validate all raw observations before deriving any cost or ratio.
    for cohort in plan:
        rows = cohorts[cohort["name"]]
        if type(rows) is not list or len(rows) != protocol["pairs"]:
            raise ValueError("counter cohort must contain every paired block")
        for row, block in zip(rows, cohort["blocks"]):
            if (type(row) is not dict or row.keys() != {"block", "order", "control", "subject"}
                    or type(row["block"]) is not int or row["block"] != block["block"]
                    or type(row["order"]) is not str or row["order"] != block["order"]):
                raise ValueError("counter block order or identity is invalid")
            for arm in ("control", "subject"):
                if lifecycle:
                    validate_composable_counter_lifecycle(row[arm])
                    engine = row[arm]["engine"]
                else:
                    validate_composable_counter_timing(row[arm])
                    engine = row[arm]["work"]["engine"]
                if engine != cohort[f"{arm}_engine"]:
                    raise ValueError("counter arm came from the wrong engine")
        admitted[cohort["name"]] = rows
    results = {}
    aa_suitable = True
    diagnostics_suitable = True
    for cohort in plan:
        name = cohort["name"]
        rows = [dict(block=row["block"], order=row["order"],
                     control=({"elapsed_ns": row["control"]["elapsed_ns"]} if lifecycle
                              else _counter_metrics(row["control"], protocol)),
                     subject=({"elapsed_ns": row["subject"]["elapsed_ns"]} if lifecycle
                              else _counter_metrics(row["subject"], protocol)))
                for row in admitted[name]]
        metrics = {metric: _counter_ratio_summary(rows, metric, protocol)
                   for metric in protocol["metrics"]}
        is_aa = cohort["control_engine"] == cohort["subject_engine"]
        for summary in metrics.values():
            diagnostics_suitable &= summary["order_suitable"] and summary["drift_suitable"]
            if is_aa:
                summary["aa_suitable"] = bool(
                    summary["available"] and summary["lower"] <= 1 <= summary["upper"]
                    and summary["upper"] / summary["lower"] <= protocol["aa_max_width"])
                aa_suitable &= summary["aa_suitable"]
        results[name] = {"metrics": metrics, "observations": rows}
    return {
        "schema": ("composable-counter-lifecycle-analysis-v1" if lifecycle
                   else "composable-counter-analysis-v1"),
        "protocol_sha256": (COUNTER_LIFECYCLE_PROTOCOL_SHA256 if lifecycle
                            else COUNTER_PROTOCOL_SHA256),
        "provenance_qualified": False,
        "family_nominal_coverage_lower_bound": protocol["family_nominal_coverage_lower_bound"],
        "coverage_assumptions": protocol["coverage_assumptions"],
        "aa_suitable": aa_suitable, "diagnostics_suitable": diagnostics_suitable,
        "suitable_for_direction": aa_suitable and diagnostics_suitable,
        "cohorts": results,
    }


def load_protocol(path: pathlib.Path) -> dict[str, Any]:
    """Load and validate the frozen composition qualification protocol."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read composition protocol: {error}") from error
    if _sha256(path) != PROTOCOL_SHA256:
        raise ValueError("composition protocol file hash is invalid")
    required = {
        "schema_version": 1,
        "method_version": "composition-paired-v6",
        "status": "frozen-before-measurement",
        "runner": RUNNER,
        "minimum_macos_major": MINIMUM_MACOS_MAJOR,
        "baseline_runner": BASELINE_RUNNER,
        "binary_baseline_runner": BINARY_BASELINE_RUNNER,
        "reference_revision": REFERENCE_REVISION,
        "budget_sha256": BUDGET_SHA256,
        "allocation_manifest_sha256": ALLOCATION_MANIFEST_SHA256,
        "regression_limit": REGRESSION_LIMIT,
    }
    for key, expected in required.items():
        if document.get(key) != expected:
            raise ValueError(f"composition protocol mismatch: {key}")

    confidence = document.get("confidence", {})
    if confidence.get("target_one_sided") != 0.95:
        raise ValueError("composition protocol confidence target is invalid")
    if confidence.get("observations_per_stratum") != 10:
        raise ValueError("composition protocol stratum size is invalid")
    if confidence.get("upper_order_statistic_rank") != 9:
        raise ValueError("composition protocol upper-bound rank is invalid")
    tail_probability = (math.comb(10, 9) + math.comb(10, 10)) / 2**10
    per_stratum_coverage = 1 - tail_probability
    joint_coverage = 1 - 2 * tail_probability
    if confidence.get("per_stratum_coverage") != per_stratum_coverage:
        raise ValueError("composition protocol stratum coverage is invalid")
    if confidence.get("finite_sample_joint_coverage") != joint_coverage:
        raise ValueError("composition protocol joint coverage is invalid")
    if joint_coverage < confidence["target_one_sided"]:
        raise ValueError("composition protocol confidence is insufficient")

    runtime = document.get("runtime", {})
    if runtime.get("pairs_per_order_stratum") != 10:
        raise ValueError("composition protocol runtime sample size is invalid")
    if runtime.get("diagnostic_timing_rows") != EXPECTED_DIVAN_ROWS:
        raise ValueError("composition protocol diagnostic row count is invalid")
    scenarios = runtime.get("gated_scenarios")
    if not isinstance(scenarios, list) or len(scenarios) != 9 or len(set(scenarios)) != 9:
        raise ValueError("composition protocol gated scenario set is invalid")

    compile_policy = document.get("compile", {})
    if compile_policy.get("pairs_per_order_stratum") != 10:
        raise ValueError("composition protocol compile sample size is invalid")
    if compile_policy.get("fixtures") != [
        "direct",
        "composed-manual",
        "composed-macro",
        "scoped",
    ]:
        raise ValueError("composition protocol compile fixtures are invalid")

    quiescence = document.get("quiescence", {})
    expected_quiescence = {
        "sample_interval_seconds": 5,
        "consecutive_samples": 7,
        "timeout_seconds": 300,
        "maximum_cpu_used_percent": 20.0,
        "maximum_load_average_1m": 6.0,
        "thermal_policy": "nsprocessinfo-nominal-and-pmset-no-recorded-warning",
        "process_monitor_interval_seconds": 1,
    }
    for key, expected in expected_quiescence.items():
        if quiescence.get(key) != expected:
            raise ValueError(f"composition protocol quiescence mismatch: {key}")
    return document


def seeded_order_schedule(protocol: dict[str, Any]) -> list[str]:
    """Return the verified deterministic, balanced block order."""
    randomisation = protocol["randomisation"]
    if randomisation.get("algorithm") != "sha256-sort-labelled-order-strata-v1":
        raise ValueError("unsupported composition order randomisation")
    seed = randomisation.get("seed")
    if not isinstance(seed, str) or not seed:
        raise ValueError("composition order seed is invalid")
    labelled = [
        (order, index)
        for order in ("control-subject", "subject-control")
        for index in range(1, 11)
    ]
    labelled.sort(
        key=lambda item: hashlib.sha256(
            f"{seed}:{item[0]}:{item[1]}".encode("utf-8")
        ).digest()
    )
    expected = [order for order, _ in labelled]
    schedule = randomisation.get("order_schedule")
    if schedule != expected:
        raise ValueError("composition order schedule does not match its seed")
    if schedule.count("control-subject") != 10 or schedule.count(
        "subject-control"
    ) != 10:
        raise ValueError("composition order schedule is not balanced")
    return list(schedule)


def measurement_plan(protocol: dict[str, Any], mode: str) -> dict[str, Any]:
    """Expand the frozen protocol into its result-independent measurement plan."""
    if mode == "candidate-reference":
        semantic_roles = {"subject": "candidate", "control": "reference"}
    elif mode == "reference-reference-aa":
        semantic_roles = {"subject": "reference-b", "control": "reference-a"}
    else:
        raise ValueError(f"unsupported paired measurement mode: {mode}")

    runtime = []
    compile_rows = []
    for block, order in enumerate(seeded_order_schedule(protocol), start=1):
        semantic_order = order.split("-")
        roles = [semantic_roles[role] for role in semantic_order]
        recorded_order = "-".join(roles)
        for scenario in protocol["runtime"]["gated_scenarios"]:
            runtime.append(
                {
                    "block": block,
                    "order": recorded_order,
                    "stratum": order,
                    "scenario": scenario,
                    "roles": roles,
                }
            )
        for fixture in protocol["compile"]["fixtures"]:
            compile_rows.append(
                {
                    "block": block,
                    "order": recorded_order,
                    "stratum": order,
                    "fixture": fixture,
                    "roles": roles,
                }
            )
    return {
        "schema_version": 1,
        "method_version": protocol["method_version"],
        "protocol_sha256": PROTOCOL_SHA256,
        "mode": mode,
        "seed": protocol["randomisation"]["seed"],
        "regression_limit": protocol["regression_limit"],
        "confidence": protocol["confidence"],
        "quiescence": protocol["quiescence"],
        "resource_counters": protocol["resource_counters"],
        "runtime": runtime,
        "compile": compile_rows,
    }


def parse_darwin_time(text: str) -> dict[str, float | int]:
    """Parse the frozen `/usr/bin/time -lp` counter set."""
    labels: dict[str, tuple[str, type[float] | type[int]]] = {
        "real": ("real_seconds", float),
        "user": ("user_seconds", float),
        "sys": ("system_seconds", float),
        "maximum resident set size": ("maximum_resident_set_bytes", int),
        "page reclaims": ("page_reclaims", int),
        "page faults": ("page_faults", int),
        "swaps": ("swaps", int),
        "block input operations": ("block_input_operations", int),
        "block output operations": ("block_output_operations", int),
        "voluntary context switches": ("voluntary_context_switches", int),
        "involuntary context switches": ("involuntary_context_switches", int),
        "instructions retired": ("instructions_retired", int),
        "cycles elapsed": ("cycles_elapsed", int),
        "peak memory footprint": ("peak_memory_footprint_bytes", int),
    }
    parsed: dict[str, float | int] = {}
    for line in text.splitlines():
        stripped = line.strip()
        for label, (field, conversion) in labels.items():
            if label in {"real", "user", "sys"}:
                match = re.fullmatch(rf"{label} ([0-9]+(?:\.[0-9]+)?)", stripped)
            else:
                match = re.fullmatch(rf"([0-9]+)\s+{re.escape(label)}", stripped)
            if match is not None:
                if field in parsed:
                    raise ValueError(f"duplicate Darwin time counter: {field}")
                parsed[field] = conversion(match.group(1))
                break
    for field, _conversion in labels.values():
        if field not in parsed:
            raise ValueError(f"missing Darwin time counter: {field}")
    return parsed


def find_competing_processes(snapshot: str, own_pid: int) -> list[dict[str, Any]]:
    """Find Cargo, compiler, or composition-benchmark processes outside our tree."""
    processes: dict[int, tuple[int, str]] = {}
    for line in snapshot.splitlines():
        fields = line.strip().split(maxsplit=2)
        if not fields:
            continue
        if len(fields) != 3:
            raise ValueError(f"invalid process snapshot row: {line}")
        try:
            pid, parent = int(fields[0]), int(fields[1])
        except ValueError as error:
            raise ValueError(f"invalid process snapshot identity: {line}") from error
        if pid <= 0 or parent < 0 or pid in processes:
            raise ValueError("invalid or duplicate process identity")
        processes[pid] = (parent, fields[2])

    if own_pid not in processes:
        raise ValueError("process snapshot does not contain the monitor")
    own_tree = {own_pid}
    changed = True
    while changed:
        changed = False
        for pid, (parent, _command) in processes.items():
            if parent in own_tree and pid not in own_tree:
                own_tree.add(pid)
                changed = True

    blocked_names = {"cargo", "cargo-nextest", "rustc", "rustdoc", "sccache",
                     "go", "compile", "link", "swift", "swiftc", "swift-frontend",
                     "clang", "clang++", "cc", "c++", "gcc", "g++", "ld",
                     "ninja", "make", "xcodebuild"}
    competitors = []
    for pid, (parent, command) in processes.items():
        basename = pathlib.PurePath(command).name
        if pid not in own_tree and (
            basename in blocked_names or basename.startswith("composition-")
        ):
            competitors.append(
                {"pid": pid, "parent_pid": parent, "command": command}
            )
    return sorted(competitors, key=lambda row: row["pid"])


def validate_quiescence_sample(
    protocol: dict[str, Any],
    *,
    cpu_used_percent: float,
    load_average_1m: float,
    thermal_text: str,
    process_snapshot: str,
    own_pid: int,
) -> dict[str, Any]:
    """Validate one host-quiescence observation against the frozen protocol."""
    policy = protocol["quiescence"]
    if any(type(value) not in (int, float) or not math.isfinite(value) or value < 0
           for value in (cpu_used_percent, load_average_1m)) or cpu_used_percent > 100:
        raise ValueError("invalid CPU or load telemetry")
    if cpu_used_percent > policy["maximum_cpu_used_percent"]:
        raise ValueError("host exceeded the frozen CPU threshold")
    if load_average_1m > policy["maximum_load_average_1m"]:
        raise ValueError("host exceeded the frozen load threshold")
    required_thermal = {
        "Note: No thermal warning level has been recorded",
        "Note: No performance warning level has been recorded",
        "Note: No CPU power status has been recorded",
    }
    if not required_thermal.issubset(set(thermal_text.splitlines())):
        raise ValueError("host thermal warning-history proxy is not canonical")
    competitors = find_competing_processes(process_snapshot, own_pid)
    if competitors:
        raise ValueError("competing Cargo, compiler, or benchmark process is running")
    return {
        "passed": True,
        "cpu_used_percent": cpu_used_percent,
        "load_average_1m": load_average_1m,
        "thermal_policy": policy["thermal_policy"],
        "competing_processes": competitors,
    }


def parse_cpu_used_percent(text: str) -> float:
    samples = [
        float(match.group(1))
        for match in re.finditer(
            r"^CPU usage: .*?([0-9]+(?:\.[0-9]+)?)% idle\s*$",
            text,
            re.MULTILINE,
        )
    ]
    if len(samples) != 2:
        raise ValueError("top evidence must contain exactly two CPU samples")
    return 100.0 - samples[-1]


def parse_load_average_1m(text: str) -> float:
    match = re.fullmatch(
        r"\s*\{?\s*([0-9]+(?:\.[0-9]+)?)\s+[0-9]+(?:\.[0-9]+)?\s+"
        r"[0-9]+(?:\.[0-9]+)?\s*\}?\s*",
        text,
    )
    if match is None:
        raise ValueError("load-average evidence is invalid")
    return float(match.group(1))


def _sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _time_to_nanoseconds(value: str, unit: str) -> float:
    factors = {
        "ps": 0.001,
        "ns": 1.0,
        "µs": 1_000.0,
        "μs": 1_000.0,
        "us": 1_000.0,
        "ms": 1_000_000.0,
        "s": 1_000_000_000.0,
    }
    return float(value) * factors[unit]


def parse_divan_transcript(path: pathlib.Path) -> dict[str, float]:
    """Return full benchmark paths mapped to their median nanoseconds."""
    if not path.is_file():
        raise ValueError(f"missing Divan transcript: {path.name}")
    text = _ANSI.sub("", path.read_text(encoding="utf-8"))
    if "fastest" not in text or "median" not in text or "samples" not in text:
        raise ValueError(f"Divan transcript lacks table header: {path}")

    groups: list[str] = []
    rows: dict[str, float] = {}
    for line in text.splitlines():
        tree = _TREE_ROW.match(line)
        if tree is None:
            continue
        prefix = tree.group("prefix")
        if len(prefix) % 3 != 0:
            raise ValueError(f"invalid Divan tree indentation in {path}: {line}")
        depth = len(prefix) // 3
        rest = tree.group("rest")
        timing = _TIMING_ROW.match(rest)
        if timing is None:
            group = rest.split("│", 1)[0].strip()
            if not group:
                raise ValueError(f"empty Divan group in {path}")
            groups = groups[:depth]
            groups.append(group)
            continue

        name = timing.group("name").strip()
        if not name:
            raise ValueError(f"empty Divan row in {path}")
        if depth > len(groups):
            raise ValueError(f"orphan Divan row in {path}: {name}")
        row = "/".join([*groups[:depth], name])
        if row in rows:
            raise ValueError(f"duplicate Divan row in {path}: {row}")
        rows[row] = _time_to_nanoseconds(
            timing.group("median"), timing.group("median_unit")
        )
    if not rows:
        raise ValueError(f"Divan transcript has no timing rows: {path}")
    return rows


def validate_divan_transcript(
    path: pathlib.Path, expected_rows: int = EXPECTED_DIVAN_ROWS
) -> dict[str, float]:
    rows = parse_divan_transcript(path)
    if len(rows) != expected_rows:
        raise ValueError(
            f"Divan transcript {path.name} has {len(rows)} rows; expected {expected_rows}"
        )
    return rows


def validate_isolated_transcript(
    path: pathlib.Path, expected_scenario: str
) -> dict[str, float]:
    rows = validate_divan_transcript(path, expected_rows=1)
    if set(rows) != {expected_scenario}:
        raise ValueError(
            f"isolated Divan scenario mismatch: expected {expected_scenario}"
        )
    return rows


def _load_resource_counters(
    path: pathlib.Path, protocol: dict[str, Any]
) -> dict[str, float | int]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read per-process resource counters: {path}") from error
    required = protocol["resource_counters"]["required"]
    if not isinstance(document, dict) or set(document) != set(required):
        raise ValueError(f"per-process resource counter set is invalid: {path}")
    for field, value in document.items():
        if (type(value) not in (int, float) or not math.isfinite(value) or value < 0
                or (not field.endswith("_seconds") and type(value) is not int)):
            raise ValueError(f"per-process resource counter is invalid: {field}")
    for field in (
        "maximum_resident_set_bytes",
        "instructions_retired",
        "cycles_elapsed",
        "peak_memory_footprint_bytes",
    ):
        if document[field] <= 0:
            raise ValueError(f"per-process resource counter must be positive: {field}")
    return document


def load_isolated_runtime_pairs(
    subject_directory: pathlib.Path,
    control_directory: pathlib.Path,
    scenario: str,
    protocol: dict[str, Any],
) -> list[dict[str, Any]]:
    """Load one isolated observation per role for every seeded block."""
    if scenario not in protocol["runtime"]["gated_scenarios"]:
        raise ValueError(f"runtime scenario is not frozen in the protocol: {scenario}")
    schedule = seeded_order_schedule(protocol)
    slug = scenario.replace("/", "__")
    expected_names = {
        f"block-{block:02d}{suffix}"
        for block in range(1, len(schedule) + 1)
        for suffix in (".txt", ".resources.json")
    }
    for directory in (subject_directory, control_directory):
        scenario_directory = directory / "runtime" / slug
        actual_names = (
            {path.name for path in scenario_directory.iterdir()}
            if scenario_directory.is_dir()
            else set()
        )
        if actual_names != expected_names:
            raise ValueError(
                f"isolated runtime evidence set is incomplete: {scenario_directory}"
            )

    pairs = []
    for block, order in enumerate(schedule, start=1):
        subject_path = subject_directory / "runtime" / slug / f"block-{block:02d}.txt"
        control_path = control_directory / "runtime" / slug / f"block-{block:02d}.txt"
        subject = validate_isolated_transcript(subject_path, scenario)[scenario]
        control = validate_isolated_transcript(control_path, scenario)[scenario]
        subject_resources = _load_resource_counters(
            subject_path.with_suffix(".resources.json"), protocol
        )
        control_resources = _load_resource_counters(
            control_path.with_suffix(".resources.json"), protocol
        )
        pairs.append(
            {
                "block": block,
                "order": order,
                "subject": subject,
                "control": control,
                "subject_transcript": str(subject_path),
                "control_transcript": str(control_path),
                "subject_resources": subject_resources,
                "control_resources": control_resources,
            }
        )
    return pairs


def load_compile_pairs(
    subject_directory: pathlib.Path,
    control_directory: pathlib.Path,
    fixture: str,
    protocol: dict[str, Any],
) -> list[dict[str, Any]]:
    """Load balanced, clean compile observations from isolated targets."""
    if fixture not in protocol["compile"]["fixtures"]:
        raise ValueError(f"compile fixture is not frozen in the protocol: {fixture}")
    schedule = seeded_order_schedule(protocol)
    expected_names = {
        f"block-{block:02d}{suffix}"
        for block in range(1, len(schedule) + 1)
        for suffix in (".txt", ".resources.json")
    }
    for directory in (subject_directory, control_directory):
        fixture_directory = directory / "compile" / fixture
        actual_names = (
            {path.name for path in fixture_directory.iterdir()}
            if fixture_directory.is_dir()
            else set()
        )
        if actual_names != expected_names:
            raise ValueError(
                f"paired compile evidence set is incomplete: {fixture_directory}"
            )

    pairs = []
    for block, order in enumerate(schedule, start=1):
        subject_path = subject_directory / "compile" / fixture / f"block-{block:02d}.txt"
        control_path = control_directory / "compile" / fixture / f"block-{block:02d}.txt"
        if not subject_path.read_text(encoding="utf-8").strip() or not control_path.read_text(
            encoding="utf-8"
        ).strip():
            raise ValueError(f"paired compile transcript is empty: {fixture} block {block}")
        subject_resources = _load_resource_counters(
            subject_path.with_suffix(".resources.json"), protocol
        )
        control_resources = _load_resource_counters(
            control_path.with_suffix(".resources.json"), protocol
        )
        pairs.append(
            {
                "block": block,
                "order": order,
                "subject": subject_resources["real_seconds"],
                "control": control_resources["real_seconds"],
                "subject_transcript": str(subject_path),
                "control_transcript": str(control_path),
                "subject_resources": subject_resources,
                "control_resources": control_resources,
            }
        )
    return pairs


def _validate_measurement_plan(
    path: pathlib.Path, protocol: dict[str, Any], mode: str
) -> dict[str, Any]:
    try:
        plan = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read frozen measurement plan: {path}") from error
    expected = measurement_plan(protocol, mode)
    if plan != expected:
        raise ValueError("measurement plan drifted from the frozen protocol")
    return plan


def validate_measurement_os(version: str) -> None:
    """Reject unknown OS versions and hosts below the current protocol minimum."""
    if (
        not re.fullmatch(r"[0-9]+(?:\.[0-9]+){0,2}", version)
        or int(version.split(".", 1)[0]) < MINIMUM_MACOS_MAJOR
    ):
        raise ValueError(f"composition measurement requires macOS {MINIMUM_MACOS_MAJOR} or later")


def _validate_protocol_pair_metadata(metadata: dict[str, Any]) -> None:
    validate_measurement_os(str(metadata.get("os_version", "")))
    if not isinstance(metadata.get("os_build"), str) or not metadata["os_build"].strip():
        raise ValueError("paired evidence OS build is missing")
    mode = metadata.get("mode")
    expected_roles = {
        "candidate-reference": ("candidate", "reference"),
        "reference-reference-aa": ("reference-b", "reference-a"),
    }
    if mode not in expected_roles:
        raise ValueError("paired evidence mode is invalid")
    if (metadata.get("subject_role"), metadata.get("control_role")) != expected_roles[mode]:
        raise ValueError("paired evidence roles do not match the mode")
    for field in ("subject_revision", "control_revision"):
        if not _REVISION.fullmatch(str(metadata.get(field, ""))):
            raise ValueError(f"paired evidence revision is invalid: {field}")
    if metadata["control_revision"] != REFERENCE_REVISION:
        raise ValueError("paired control revision is not the frozen reference")
    if mode == "candidate-reference":
        if metadata["subject_revision"] == REFERENCE_REVISION:
            raise ValueError("candidate and reference revisions must differ")
    elif metadata["subject_revision"] != REFERENCE_REVISION:
        raise ValueError("A/A evidence must use the frozen reference twice")


def evaluate_protocol_paired(
    budget_path: pathlib.Path,
    protocol: dict[str, Any],
    subject_directory: pathlib.Path,
    control_directory: pathlib.Path,
    plan_path: pathlib.Path,
    metadata: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    """Evaluate isolated runtime and contemporaneous clean-compile pairs."""
    _validate_protocol_pair_metadata(metadata)
    _validate_measurement_plan(plan_path, protocol, metadata["mode"])
    document = validate_budget_document(budget_path)

    subject_rows = []
    control_rows = []
    comparisons = []
    loaded_runtime: dict[str, list[dict[str, Any]]] = {}
    for budget_row in _runtime_rows(document):
        scenario = budget_row["scenario"].removeprefix("runtime/")
        if scenario not in loaded_runtime:
            loaded_runtime[scenario] = load_isolated_runtime_pairs(
                subject_directory, control_directory, scenario, protocol
            )
        pairs = loaded_runtime[scenario]
        direction = _direction(budget_row)
        converted = []
        for pair in pairs:
            if budget_row["measured_statistic"] == "median_throughput_across_processes":
                subject_value = float(
                    round((32 * 1_000_000_000 / pair["subject"]) / 100) * 100
                )
                control_value = float(
                    round((32 * 1_000_000_000 / pair["control"]) / 100) * 100
                )
            elif budget_row["measured_statistic"] == "median_of_process_medians":
                subject_value = pair["subject"]
                control_value = pair["control"]
            else:
                raise ValueError(
                    f"unsupported runtime statistic: {budget_row['measured_statistic']}"
                )
            converted.append(
                {
                    **pair,
                    "subject": subject_value,
                    "control": control_value,
                }
            )
        statistics_result = paired_ratio_statistics(converted, direction, protocol)
        identity = {
            "scenario": budget_row["scenario"],
            "measured_statistic": budget_row["measured_statistic"],
            "units": budget_row["units"],
            "direction": direction,
        }
        subject_rows.append(
            {
                **identity,
                "process_values": [
                    {
                        "block": pair["block"],
                        "order_stratum": pair["order"],
                        "value": pair["subject"],
                        "transcript": pair["subject_transcript"],
                        "resources": pair["subject_resources"],
                    }
                    for pair in converted
                ],
            }
        )
        control_rows.append(
            {
                **identity,
                "process_values": [
                    {
                        "block": pair["block"],
                        "order_stratum": pair["order"],
                        "value": pair["control"],
                        "transcript": pair["control_transcript"],
                        "resources": pair["control_resources"],
                    }
                    for pair in converted
                ],
            }
        )
        comparisons.append({**identity, "pairs": converted, **statistics_result})

    compile_budgets = {
        row["scenario"].split("/")[1]: row
        for row in document["rows"]
        if row["scenario"].startswith("compile/")
    }
    for fixture in protocol["compile"]["fixtures"]:
        budget_row = compile_budgets[fixture]
        pairs = load_compile_pairs(
            subject_directory, control_directory, fixture, protocol
        )
        statistics_result = paired_ratio_statistics(pairs, "maximum", protocol)
        identity = {
            "scenario": budget_row["scenario"],
            "measured_statistic": budget_row["measured_statistic"],
            "units": budget_row["units"],
            "direction": "maximum",
        }
        subject_rows.append(
            {
                **identity,
                "process_values": [
                    {
                        "block": pair["block"],
                        "order_stratum": pair["order"],
                        "value": pair["subject"],
                        "transcript": pair["subject_transcript"],
                        "resources": pair["subject_resources"],
                    }
                    for pair in pairs
                ],
            }
        )
        control_rows.append(
            {
                **identity,
                "process_values": [
                    {
                        "block": pair["block"],
                        "order_stratum": pair["order"],
                        "value": pair["control"],
                        "transcript": pair["control_transcript"],
                        "resources": pair["control_resources"],
                    }
                    for pair in pairs
                ],
            }
        )
        comparisons.append({**identity, "pairs": pairs, **statistics_result})

    common = {
        "schema_version": 2,
        "suite": "rca-composition-protocol-paired",
        "mode": metadata["mode"],
        "runner": RUNNER,
        "toolchain": TOOLCHAIN,
        "method_version": protocol["method_version"],
        "protocol_sha256": PROTOCOL_SHA256,
        "metadata": metadata,
    }
    subject_report = {
        **common,
        "role": metadata["subject_role"],
        "revision": metadata["subject_revision"],
        "paired_role": metadata["control_role"],
        "paired_revision": metadata["control_revision"],
        "rows": subject_rows,
    }
    control_report = {
        **common,
        "role": metadata["control_role"],
        "revision": metadata["control_revision"],
        "paired_role": metadata["subject_role"],
        "paired_revision": metadata["subject_revision"],
        "rows": control_rows,
    }
    comparison = {
        **common,
        "subject_role": metadata["subject_role"],
        "control_role": metadata["control_role"],
        "subject_revision": metadata["subject_revision"],
        "control_revision": metadata["control_revision"],
        "method": protocol["confidence"],
        "rows": comparisons,
        "failed_gates": [
            {
                "scenario": row["scenario"],
                "measured_statistic": row["measured_statistic"],
            }
            for row in comparisons
            if not row["passed"]
        ],
        "passed": all(row["passed"] for row in comparisons),
    }
    return subject_report, control_report, comparison


def validate_protocol_diagnostics(
    directory: pathlib.Path,
    roles: tuple[str, str],
    protocol: dict[str, Any],
) -> None:
    expected = {
        f"{role}{suffix}"
        for role in roles
        for suffix in (".txt", ".resources.json")
    }
    actual = {path.name for path in directory.iterdir()} if directory.is_dir() else set()
    if actual != expected:
        raise ValueError("full Divan diagnostic evidence set is incomplete")
    for role in roles:
        transcript = directory / f"{role}.txt"
        validate_divan_transcript(
            transcript, protocol["runtime"]["diagnostic_timing_rows"]
        )
        _load_resource_counters(transcript.with_suffix(".resources.json"), protocol)


def _load_protocol_metadata(
    path: pathlib.Path, protocol: dict[str, Any], plan_path: pathlib.Path
) -> dict[str, Any]:
    try:
        metadata = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read paired protocol metadata: {path}") from error
    if not isinstance(metadata, dict):
        raise ValueError("paired protocol metadata must be an object")
    _validate_protocol_pair_metadata(metadata)
    expected = {
        "schema_version": 1,
        "method_version": protocol["method_version"],
        "protocol_sha256": PROTOCOL_SHA256,
        "budget_sha256": BUDGET_SHA256,
        "allocation_manifest_sha256": ALLOCATION_MANIFEST_SHA256,
        "seed": protocol["randomisation"]["seed"],
        "runner": RUNNER,
        "toolchain": TOOLCHAIN,
        "plan_sha256": _sha256(plan_path),
    }
    for field, value in expected.items():
        if metadata.get(field) != value:
            raise ValueError(f"paired protocol metadata mismatch: {field}")
    if not _TIMESTAMP.fullmatch(str(metadata.get("recorded_at", ""))):
        raise ValueError("paired protocol metadata timestamp is invalid")
    for field in (
        "subject_lock_sha256",
        "control_lock_sha256",
        "qualification_script_sha256",
        "evaluator_script_sha256",
        "capture_script_sha256",
        "workload_sha256",
        "benchmark_list_sha256",
        "subject_executable_sha256",
        "control_executable_sha256",
    ):
        if not _HASH.fullmatch(str(metadata.get(field, ""))):
            raise ValueError(f"paired protocol metadata hash is invalid: {field}")
    if metadata["workload_sha256"] != protocol["common_workloads"]["files"][
        "rca-core/benches/composition.rs"]["sha256"]:
        raise ValueError("paired common benchmark hash differs from the protocol")
    workload_paths = {row["workload_source"] for row in validate_budget_document(
        pathlib.Path(__file__).resolve().parents[1] / "docs/performance/composition/composition-budgets.json")["rows"]}
    for role in ("subject", "control"):
        provenance = metadata.get("workload_provenance", {}).get(role, {})
        for kind in ("original", "effective"):
            hashes = provenance.get(kind, {})
            if set(hashes) != workload_paths or any(not _HASH.fullmatch(str(value)) for value in hashes.values()):
                raise ValueError(f"paired workload provenance is incomplete: {role} {kind}")
        if not _HASH.fullmatch(str(provenance.get("receipt_sha256", ""))):
            raise ValueError(f"paired source receipt hash is invalid: {role}")
    if metadata.get("zero_host_sha256") != protocol["common_workloads"]["files"][
        "rca-core/tests/composition_qualification/main.rs"]["sha256"]:
        raise ValueError("paired zero host hash differs from the protocol")
    return metadata


def validate_protocol_sources(directory: pathlib.Path, metadata: dict[str, Any], protocol: dict[str, Any]) -> None:
    """Authenticate source receipts without requiring disposable source archives to remain."""
    files = protocol["common_workloads"]["files"]
    allowed = files.keys() | {"rca-core/Cargo.toml"}
    for role in ("subject", "control"):
        path = directory / "sources" / f"{role}.json"
        declared = metadata["workload_provenance"][role]
        if _sha256(path) != declared["receipt_sha256"]:
            raise ValueError(f"paired source receipt hash mismatch: {role}")
        receipt = json.loads(path.read_text())
        if (receipt.get("revision") != metadata[f"{role}_revision"]
                or not _REVISION.fullmatch(str(receipt.get("tree", "")))
                or receipt.get("common_workloads") != files):
            raise ValueError(f"paired source identity mismatch: {role}")
        expected_adapters = ["external-zero-host-v1"]
        if receipt["revision"] != REFERENCE_REVISION:
            expected_adapters.append("unit-projection-constructor-v1")
        if receipt.get("adapters") != expected_adapters:
            raise ValueError(f"paired source adapters mismatch: {role}")
        for kind in ("original", "effective"):
            hashes = receipt.get(kind, {})
            if any(not _HASH.fullmatch(str(value)) for value in hashes.values()) or any(
                hashes.get(name) != digest for name, digest in declared[kind].items()
            ):
                raise ValueError(f"paired source provenance mismatch: {role} {kind}")
        changed = {name for name in receipt["original"].keys() | receipt["effective"].keys()
                   if receipt["original"].get(name) != receipt["effective"].get(name)}
        if changed - allowed:
            raise ValueError(f"paired source replaced product implementation: {role}")
        for name, row in files.items():
            expected = row["sha256"]
            if name == "examples/composition-search/tests/app.rs" and receipt["revision"] != REFERENCE_REVISION:
                expected = protocol["common_workloads"]["candidate_app_sha256"]
            if receipt["effective"].get(name) != expected:
                raise ValueError(f"paired effective workload hash mismatch: {role} {name}")


def _direction(row: dict[str, Any]) -> str:
    if row["measured_statistic"] == "median_throughput_across_processes":
        return "minimum"
    if row["absolute_budget"] == 0:
        return "exact"
    return "maximum"


def _runtime_rows(document: dict[str, Any]) -> list[dict[str, Any]]:
    return [row for row in document["rows"] if row["scenario"].startswith("runtime/")]


def validate_allocation_manifest(path: pathlib.Path) -> dict[str, dict[str, int]]:
    if not path.is_file():
        raise ValueError(f"missing allocation workload manifest: {path}")
    if _sha256(path) != ALLOCATION_MANIFEST_SHA256:
        raise ValueError("allocation workload manifest hash is invalid")

    workloads: dict[str, dict[str, int]] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        fields = line.split()
        if len(fields) != 3:
            raise ValueError(f"invalid allocation workload manifest row: {line}")
        scenario, samples_text, logical_operations_text = fields
        try:
            samples = int(samples_text)
            logical_operations = int(logical_operations_text)
        except ValueError as error:
            raise ValueError(f"invalid allocation workload counts: {scenario}") from error
        if scenario in workloads or samples <= 0 or logical_operations <= 0:
            raise ValueError(f"invalid allocation workload identity: {scenario}")
        workloads[scenario] = {
            "samples": samples,
            "logical_operations": logical_operations,
        }
    if len(workloads) != EXPECTED_ALLOCATIONS:
        raise ValueError(
            f"allocation workload manifest has {len(workloads)} rows; "
            f"expected {EXPECTED_ALLOCATIONS}"
        )
    return workloads


def validate_budget_document(
    path: pathlib.Path,
    expected_reference: str = REFERENCE_REVISION,
    candidate_root: pathlib.Path | None = None,
    reference_root: pathlib.Path | None = None,
) -> dict[str, Any]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read composition budgets: {error}") from error

    if document.get("schema_version") != 2:
        raise ValueError("composition budget schema must be 2")
    if document.get("baseline_kind") != "first-absolute-composition-baseline":
        raise ValueError("composition budget kind is invalid")
    if document.get("measured_revision") != expected_reference:
        raise ValueError("composition budget reference revision is invalid")
    if document.get("runner_cohort") != BASELINE_RUNNER:
        raise ValueError("composition budget runner is invalid")
    if document.get("toolchain") != TOOLCHAIN:
        raise ValueError("composition budget toolchain is invalid")
    if not _HASH.fullmatch(str(document.get("runner_source_hash", ""))):
        raise ValueError("composition budget runner source hash is invalid")

    rows = document.get("rows")
    if not isinstance(rows, list) or len(rows) != 33:
        raise ValueError("composition budgets must contain exactly 33 rows")
    counts = {"runtime": 0, "allocation": 0, "compile": 0, "binary": 0}
    identities: set[tuple[str, str]] = set()
    for row in rows:
        if not isinstance(row, dict):
            raise ValueError("composition budget row must be an object")
        scenario = row.get("scenario")
        statistic = row.get("measured_statistic")
        if not isinstance(scenario, str) or not isinstance(statistic, str):
            raise ValueError("composition budget row identity is invalid")
        identity = (scenario, statistic)
        if identity in identities:
            raise ValueError(f"duplicate composition budget row: {scenario} ({statistic})")
        identities.add(identity)
        category = scenario.split("/", 1)[0]
        if category not in counts:
            raise ValueError(f"unsupported composition budget category: {category}")
        counts[category] += 1
        expected_cohort = BINARY_BASELINE_RUNNER if category == "binary" else BASELINE_RUNNER
        if row.get("runner_cohort") != expected_cohort or row.get("toolchain") != TOOLCHAIN:
            raise ValueError(f"budget cohort drift: {scenario}")
        if row.get("measured_revision") != expected_reference:
            raise ValueError(f"budget revision drift: {scenario}")
        if row.get("diagnostic_only") is not False:
            raise ValueError(f"release budget cannot be diagnostic-only: {scenario}")
        if not isinstance(row.get("absolute_budget"), (int, float)):
            raise ValueError(f"budget value is invalid: {scenario}")
        source = row.get("workload_source")
        source_hash = row.get("workload_source_hash")
        if not isinstance(source, str) or not _HASH.fullmatch(str(source_hash or "")):
            raise ValueError(f"workload identity is invalid: {scenario}")
        for root in (candidate_root, reference_root):
            if root is None:
                continue
            workload = root / source
            if not workload.is_file():
                raise ValueError(f"missing workload source: {workload}")
            if _sha256(workload) != source_hash:
                raise ValueError(f"workload source hash mismatch: {workload}")
    if counts != {"runtime": 10, "allocation": 13, "compile": 4, "binary": 6}:
        raise ValueError(f"composition budget category counts are invalid: {counts}")
    if _sha256(path) != BUDGET_SHA256:
        raise ValueError("composition budget file hash is invalid")
    _validate_binary_baseline(document, path.parent)
    return document


def _validate_binary_baseline(document: dict[str, Any], root: pathlib.Path) -> None:
    """Keep the corrected binary-only reference separate from the historical cohort."""
    provenance = document.get("binary_baseline", {})
    required = {
        "runner_cohort": BINARY_BASELINE_RUNNER,
        "os_version": "27.0",
        "os_build": "26A428",
        "measured_revision": REFERENCE_REVISION,
        "toolchain": TOOLCHAIN,
    }
    for key, expected in required.items():
        if provenance.get(key) != expected:
            raise ValueError(f"binary baseline provenance mismatch: {key}")
    for key in ("binary_sizes", "verification", "driver"):
        receipt = provenance.get(key, {})
        relative = pathlib.Path(receipt.get("path", ""))
        if relative.is_absolute() or ".." in relative.parts or not relative.parts:
            raise ValueError(f"binary baseline receipt path is invalid: {key}")
        path = root / relative
        if not path.is_file() or _sha256(path) != receipt.get("sha256"):
            raise ValueError(f"binary baseline receipt hash mismatch: {key}")
    binaries = _parse_binaries(root / provenance["binary_sizes"]["path"])
    for row in document["rows"]:
        if not row["scenario"].startswith("binary/"):
            continue
        name = row["scenario"].split("/", 1)[1]
        baseline = binaries[name]["bytes"]
        if (
            row["baseline_value"] != baseline
            or row["tolerance"] != 0.1
            or row["absolute_budget"] != (baseline * 11 + 9) // 10
        ):
            raise ValueError(f"binary baseline budget mismatch: {name}")


def _parse_key_values(path: pathlib.Path) -> dict[str, str]:
    if not path.is_file():
        raise ValueError(f"missing evidence file: {path.name}")
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        if not key or key in values:
            raise ValueError(f"duplicate or empty evidence key in {path}: {key}")
        values[key] = value
    return values


def _validate_environment(
    raw_directory: pathlib.Path,
    expected_revision: str,
    capture_script_sha256: str | None,
    root_lock_sha256: str | None,
    paired_host: dict[str, Any] | None = None,
) -> dict[str, str]:
    environment = _parse_key_values(raw_directory / "environment.txt")
    required = {
        "measured_revision": expected_revision,
        "dirty_state": "clean",
        "architecture": "arm64",
        "machine_model": "Mac15,9",
        "cpu_model": "Apple M3 Max",
        "logical_cpus": "16",
        "memory_bytes": "51539607552",
        "cargo_version": "cargo 1.98.0 (797e8a9bc 2026-08-05)",
        "cargo_path": "<toolchain>/cargo",
        "rustc_path": "<toolchain>/rustc",
        "rustdoc_path": "<toolchain>/rustdoc",
        "rustc_wrapper": "disabled",
        "rustc_workspace_wrapper": "disabled",
        "bench_profile_override": "lto=false,debug=false,strip=true",
        "runtime_process_repetitions": "10",
        "runtime_expected_timing_rows": "153",
    }
    for key, expected in required.items():
        if environment.get(key) != expected:
            raise ValueError(f"absolute capture environment mismatch: {key}")
    if paired_host is not None:
        validate_measurement_os(environment.get("os_version", ""))
        for key in ("os_version", "os_build"):
            if environment.get(key) != paired_host.get(key):
                raise ValueError(f"absolute capture differs from paired host: {key}")
    elif environment.get("os_version", "").split(".", 1)[0] != "26":
        raise ValueError("absolute capture requires macOS 26")
    if "AC Power" not in environment.get("power_source", ""):
        raise ValueError("absolute capture requires AC power")
    if "powermode" not in environment.get("power_mode", "") or "2" not in environment.get(
        "power_mode", ""
    ):
        raise ValueError("absolute capture requires high-power mode")
    for key in ("recorded_at", "completed_at"):
        if not _TIMESTAMP.fullmatch(environment.get(key, "")):
            raise ValueError(f"absolute capture timestamp is invalid: {key}")
    for key in (
        "root_lock_sha256",
        "fixture_lock_sha256",
        "benchmark_source_sha256",
        "scope_zero_test_source_sha256",
        "combine_zero_test_source_sha256",
        "runner_source_sha256",
    ):
        if not _HASH.fullmatch(environment.get(key, "")):
            raise ValueError(f"absolute capture hash is invalid: {key}")
    if capture_script_sha256 is not None and environment["runner_source_sha256"] != capture_script_sha256:
        raise ValueError("absolute capture script hash mismatch")
    if root_lock_sha256 is not None and environment["root_lock_sha256"] != root_lock_sha256:
        raise ValueError("absolute capture lockfile hash mismatch")
    return environment


def _runtime_value(row: dict[str, Any], transcript: dict[str, float]) -> float:
    benchmark = row["scenario"].removeprefix("runtime/")
    try:
        duration = transcript[benchmark]
    except KeyError as error:
        raise ValueError(f"missing Divan budget row: {benchmark}") from error
    if row["measured_statistic"] == "median_throughput_across_processes":
        return float(round((32 * 1_000_000_000 / duration) / 100) * 100)
    if row["measured_statistic"] != "median_of_process_medians":
        raise ValueError(f"unsupported runtime statistic: {row['measured_statistic']}")
    return duration


def _parse_allocations(
    path: pathlib.Path, workload_manifest: pathlib.Path
) -> dict[str, dict[str, int]]:
    if not path.is_file():
        raise ValueError(f"missing allocation evidence: {path.name}")
    rows: dict[str, dict[str, int]] = {}
    pattern = re.compile(
        r"^scenario=(\S+) samples=(\d+) logical_operations=(\d+) "
        r"allocations=(\d+) allocated_bytes=(\d+) retained_bytes=(\d+) "
        r"peak_live_bytes=(\d+)$"
    )
    for line in path.read_text(encoding="utf-8").splitlines():
        match = pattern.fullmatch(line)
        if match is None:
            if line.strip():
                raise ValueError(f"invalid allocation evidence row: {line}")
            continue
        scenario = match.group(1)
        if scenario in rows:
            raise ValueError(f"duplicate allocation evidence row: {scenario}")
        rows[scenario] = {
            "samples": int(match.group(2)),
            "logical_operations": int(match.group(3)),
            "allocations": int(match.group(4)),
            "allocated_bytes": int(match.group(5)),
            "retained_bytes": int(match.group(6)),
            "peak_live_bytes": int(match.group(7)),
        }
    if len(rows) != EXPECTED_ALLOCATIONS:
        raise ValueError(
            f"allocation evidence has {len(rows)} rows; expected {EXPECTED_ALLOCATIONS}"
        )
    expected = validate_allocation_manifest(workload_manifest)
    if set(rows) != set(expected):
        raise ValueError("allocation workload mismatch: scenario set")
    for scenario, policy in expected.items():
        for field in ("samples", "logical_operations"):
            if rows[scenario][field] != policy[field]:
                raise ValueError(
                    f"allocation workload mismatch: {scenario} {field}"
                )
    return rows


def _validate_zero_test(path: pathlib.Path, test_name: str) -> None:
    if not path.is_file():
        raise ValueError(f"missing zero-allocation evidence: {path.name}")
    text = path.read_text(encoding="utf-8")
    if test_name not in text or "test result: ok. 1 passed; 0 failed" not in text:
        raise ValueError(f"zero-allocation gate did not pass: {path.name}")


def _parse_compile(path: pathlib.Path) -> list[float]:
    if not path.is_file():
        raise ValueError(f"missing compile evidence: {path.name}")
    phases: dict[tuple[int, str], float] = {}
    current: tuple[int, str] | None = None
    for line in path.read_text(encoding="utf-8").splitlines():
        phase = re.fullmatch(r"sample=(\d+) phase=(clean|incremental)", line)
        if phase is not None:
            current = (int(phase.group(1)), phase.group(2))
            if current in phases:
                raise ValueError(f"duplicate compile phase in {path}: {current}")
            continue
        elapsed = re.fullmatch(r"real ([0-9]+(?:\.[0-9]+)?)", line)
        if elapsed is not None and current is not None:
            phases[current] = float(elapsed.group(1))
            current = None
    expected = {(sample, phase) for sample in range(1, 6) for phase in ("clean", "incremental")}
    if set(phases) != expected:
        raise ValueError(f"compile evidence phases are incomplete: {path.name}")
    return [phases[(sample, "clean")] for sample in range(1, 6)]


def _parse_binaries(path: pathlib.Path) -> dict[str, dict[str, Any]]:
    if not path.is_file():
        raise ValueError(f"missing binary evidence: {path.name}")
    rows: dict[str, dict[str, Any]] = {}
    current: str | None = None
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith("binary="):
            current = line.removeprefix("binary=")
            if not current or current in rows:
                raise ValueError(f"duplicate binary evidence: {current}")
            rows[current] = {}
        elif current is not None and line.startswith("bytes="):
            rows[current]["bytes"] = int(line.removeprefix("bytes="))
        elif current is not None and line.startswith("sha256="):
            value = line.removeprefix("sha256=")
            if not _HASH.fullmatch(value):
                raise ValueError(f"invalid binary hash: {current}")
            rows[current]["sha256"] = value
        elif current is not None and line.startswith("identity="):
            rows[current]["identity"] = line.removeprefix("identity=")
    if len(rows) != 6 or any(set(row) != {"bytes", "sha256", "identity"} for row in rows.values()):
        raise ValueError("binary evidence must contain six complete rows")
    return rows


def _passes(value: float, budget: float, direction: str) -> bool:
    if direction == "minimum":
        return value >= budget
    if direction == "maximum":
        return value <= budget
    if direction == "exact":
        return value == budget
    raise ValueError(f"unsupported budget direction: {direction}")


def evaluate_absolute(
    budget_path: pathlib.Path,
    raw_directory: pathlib.Path,
    expected_revision: str | None,
    capture_script_sha256: str | None = None,
    root_lock_sha256: str | None = None,
    allocation_manifest: pathlib.Path = DEFAULT_ALLOCATION_MANIFEST,
    timing_report: dict[str, Any] | None = None,
) -> dict[str, Any]:
    document = validate_budget_document(budget_path)
    revision = expected_revision or document["measured_revision"]
    paired_host = None
    if timing_report is not None:
        if timing_report.get("revision") != revision:
            raise ValueError("absolute timing report revision mismatch")
        if (
            timing_report.get("protocol_sha256") != PROTOCOL_SHA256
            or timing_report.get("method_version") != "composition-paired-v6"
            or timing_report.get("runner") != RUNNER
        ):
            raise ValueError("absolute timing report protocol mismatch")
        paired_host = timing_report["metadata"]
        _validate_protocol_pair_metadata(paired_host)
        if paired_host["subject_revision"] != revision:
            raise ValueError("absolute timing report revision mismatch")
        if timing_report.get("role") != paired_host["subject_role"]:
            raise ValueError("absolute timing report must use the subject arm")
    environment = _validate_environment(
        raw_directory, revision, capture_script_sha256, root_lock_sha256, paired_host
    )
    validate_allocation_manifest(allocation_manifest)
    captured_manifest_hash = environment.get("allocation_manifest_sha256")
    if captured_manifest_hash not in (None, ALLOCATION_MANIFEST_SHA256):
        raise ValueError("absolute capture allocation manifest hash mismatch")
    if revision != REFERENCE_REVISION and captured_manifest_hash is None:
        raise ValueError("absolute capture allocation manifest hash is missing")
    workload_environment_keys = {
        "rca-core/benches/composition.rs": "benchmark_source_sha256",
        "rca-core/src/composition/scope.rs": "scope_zero_test_source_sha256",
        "rca-core/src/composition/combine.rs": "combine_zero_test_source_sha256",
    }
    for source, environment_key in workload_environment_keys.items():
        expected_hashes = {
            row["workload_source_hash"]
            for row in document["rows"]
            if row["workload_source"] == source
        }
        if paired_host is not None:
            expected_hashes = {paired_host["workload_provenance"]["subject"]["effective"][source]}
        if len(expected_hashes) != 1 or environment[environment_key] not in expected_hashes:
            raise ValueError(f"absolute capture workload hash mismatch: {source}")
    if paired_host is not None and environment.get("zero_host_sha256") != paired_host["zero_host_sha256"]:
        raise ValueError("absolute capture zero host hash mismatch")
    timing_rows = {}
    if timing_report is not None:
        for row in timing_report["rows"]:
            identity = (row["scenario"], row["measured_statistic"])
            if identity in timing_rows:
                raise ValueError("duplicate absolute timing row")
            values = row["process_values"]
            if len(values) != 20 or [value["block"] for value in values] != list(range(1, 21)):
                raise ValueError("absolute timing requires twenty ordered observations")
            if any(type(value["value"]) not in (int, float) or not math.isfinite(value["value"]) or value["value"] <= 0 for value in values):
                raise ValueError("absolute timing values must be finite and positive")
            timing_rows[identity] = row
        expected = {(row["scenario"], row["measured_statistic"]) for row in document["rows"] if row["scenario"].startswith(("runtime/", "compile/"))}
        if set(timing_rows) != expected:
            raise ValueError("absolute timing row set is incomplete")
    transcripts = _complete_transcripts(raw_directory) if timing_report is None else []
    allocation_rows = _parse_allocations(
        raw_directory / "allocations.txt", allocation_manifest
    )
    _validate_zero_test(
        raw_directory / "zero-direct-scope.txt",
        "scope_none_paths_do_not_allocate_after_warm_up",
    )
    _validate_zero_test(
        raw_directory / "zero-combine.txt", "combine_all_none_widths_do_not_allocate"
    )
    compile_rows = {} if timing_report is not None else {
        fixture: _parse_compile(raw_directory / f"compile-{fixture}.txt")
        for fixture in ("direct", "composed-manual", "composed-macro", "scoped")
    }
    binary_rows = _parse_binaries(raw_directory / "binary-sizes.txt")

    results: list[dict[str, Any]] = []
    for budget_row in document["rows"]:
        scenario = budget_row["scenario"]
        category, name = scenario.split("/", 1)
        statistic = budget_row["measured_statistic"]
        samples: list[float] | None = None
        if category in ("runtime", "compile") and timing_report is not None:
            samples = [value["value"] for value in timing_rows[(scenario, statistic)]["process_values"]]
            value = float(statistics.median(samples))
        elif category == "runtime":
            samples = [_runtime_value(budget_row, transcript) for transcript in transcripts]
            value = float(statistics.median(samples))
        elif category == "allocation":
            if name in {
                "direct_warmed_synchronous",
                "required_scope_warmed_synchronous",
                "combine_empty_effect_arities_1_through_12",
            }:
                value = 0.0
            else:
                allocation_name, allocation_statistic = name.rsplit("/", 1)
                if allocation_statistic != statistic:
                    raise ValueError(f"allocation statistic mismatch: {scenario}")
                try:
                    value = float(allocation_rows[allocation_name][allocation_statistic])
                except KeyError as error:
                    raise ValueError(f"missing allocation budget row: {scenario}") from error
        elif category == "compile":
            fixture, phase = name.rsplit("/", 1)
            if phase != "clean" or statistic != "median_elapsed":
                raise ValueError(f"unsupported compile budget row: {scenario}")
            samples = compile_rows[fixture]
            value = float(statistics.median(samples))
        elif category == "binary":
            if statistic != "stripped_file_size":
                raise ValueError(f"unsupported binary budget row: {scenario}")
            try:
                value = float(binary_rows[name]["bytes"])
            except KeyError as error:
                raise ValueError(f"missing binary budget row: {scenario}") from error
        else:
            raise ValueError(f"unsupported budget category: {category}")

        direction = _direction(budget_row)
        result = {
            "scenario": scenario,
            "measured_statistic": statistic,
            "units": budget_row["units"],
            "workload_source": budget_row["workload_source"],
            "workload_source_sha256": budget_row["workload_source_hash"],
            "direction": direction,
            "measured_value": value,
            "baseline_runner": budget_row["runner_cohort"],
            "absolute_budget": budget_row["absolute_budget"],
            "passed": _passes(value, float(budget_row["absolute_budget"]), direction),
        }
        if paired_host is not None:
            result["baseline_workload_source"] = result["workload_source"]
            result["baseline_workload_source_sha256"] = result["workload_source_sha256"]
            result["workload_source_sha256"] = paired_host["workload_provenance"]["subject"][
                "effective"][budget_row["workload_source"]]
            if category == "allocation" and name in {
                "direct_warmed_synchronous", "required_scope_warmed_synchronous",
                "combine_empty_effect_arities_1_through_12"
            }:
                result["product_source_sha256"] = result["workload_source_sha256"]
                result["workload_source"] = "rca-core/tests/composition_qualification/main.rs"
                result["workload_source_sha256"] = paired_host["zero_host_sha256"]
                result["zero_host_sha256"] = paired_host["zero_host_sha256"]
        if samples is not None:
            result["process_values"] = samples
        if category in ("runtime", "compile") and timing_report is not None:
            result["raw_evidence"] = [value["transcript"] for value in timing_rows[(scenario, statistic)]["process_values"]]
        elif category == "runtime":
            result["raw_evidence"] = [
                f"raw/candidate-absolute/runtime-divan-{repetition:02d}.txt"
                for repetition in range(1, 11)
            ]
        elif category == "allocation":
            if name in {"direct_warmed_synchronous", "required_scope_warmed_synchronous"}:
                result["raw_evidence"] = ["raw/candidate-absolute/zero-direct-scope.txt"]
            elif name == "combine_empty_effect_arities_1_through_12":
                result["raw_evidence"] = ["raw/candidate-absolute/zero-combine.txt"]
            else:
                result["raw_evidence"] = ["raw/candidate-absolute/allocations.txt"]
        elif category == "compile":
            result["raw_evidence"] = [
                f"raw/candidate-absolute/compile-{fixture}.txt"
            ]
        elif category == "binary":
            result["raw_evidence"] = ["raw/candidate-absolute/binary-sizes.txt"]
        results.append(result)

    return {
        "schema_version": 1,
        "suite": "rca-composition-absolute-budgets",
        "timing_method": "composition-paired-v6-subject-arm" if timing_report is not None else "historical-task4",
        "candidate_revision": revision,
        "reference_revision": document["measured_revision"],
        "runner": RUNNER if timing_report is not None else document["runner_cohort"],
        "baseline_runner": document["runner_cohort"],
        "binary_baseline_runner": document["binary_baseline"]["runner_cohort"],
        "toolchain": document["toolchain"],
        "capture": {
            "os_version": environment["os_version"],
            "os_build": environment["os_build"],
            "recorded_at": environment.get("recorded_at"),
            "completed_at": environment.get("completed_at"),
            "root_lock_sha256": environment["root_lock_sha256"],
            "fixture_lock_sha256": environment["fixture_lock_sha256"],
            "capture_script_sha256": environment["runner_source_sha256"],
            "workload_sha256": environment["benchmark_source_sha256"],
            "allocation_manifest_sha256": ALLOCATION_MANIFEST_SHA256,
        },
        "rows": results,
        "failed_budgets": [
            {
                "scenario": row["scenario"],
                "measured_statistic": row["measured_statistic"],
            }
            for row in results
            if not row["passed"]
        ],
        "passed": all(row["passed"] for row in results),
    }






def paired_ratio_statistics(
    repetitions: list[dict[str, Any]],
    direction: str,
    protocol: dict[str, Any],
) -> dict[str, Any]:
    """Compute the point estimate and exact distribution-free upper bound."""
    schedule = seeded_order_schedule(protocol)
    if len(repetitions) != len(schedule):
        raise ValueError("paired runtime gate requires twenty seeded blocks")
    expected_blocks = list(range(1, len(schedule) + 1))
    if [row.get("block") for row in repetitions] != expected_blocks:
        raise ValueError("paired runtime blocks are not ordered 1 through 20")
    if [row.get("order") for row in repetitions] != schedule:
        raise ValueError("paired runtime blocks do not match the frozen seeded order")

    ratios_by_order = {
        "control-subject": [],
        "subject-control": [],
    }
    for row in repetitions:
        control = row.get("control", 0)
        subject = row.get("subject", 0)
        if any(type(value) not in (int, float) or not math.isfinite(value) or value <= 0
               for value in (control, subject)):
            raise ValueError("paired runtime values must be positive")
        if direction == "minimum":
            ratio = control / subject
        elif direction == "maximum":
            ratio = subject / control
        else:
            raise ValueError(
                f"relative gate direction must be minimum or maximum: {direction}"
            )
        ratios_by_order[row["order"]].append(ratio)

    observations = protocol["confidence"]["observations_per_stratum"]
    upper_rank = protocol["confidence"]["upper_order_statistic_rank"]
    if any(len(values) != observations for values in ratios_by_order.values()):
        raise ValueError("paired runtime order strata are not balanced")
    medians = {
        order: statistics.median(values)
        for order, values in ratios_by_order.items()
    }
    upper_bounds = {
        order: sorted(values)[upper_rank - 1]
        for order, values in ratios_by_order.items()
    }
    point_estimate = math.sqrt(math.prod(medians.values()))
    upper_confidence_bound = math.sqrt(math.prod(upper_bounds.values()))
    return {
        "point_estimate": point_estimate,
        "upper_confidence_bound": upper_confidence_bound,
        "confidence_level": protocol["confidence"]["target_one_sided"],
        "finite_sample_joint_coverage": protocol["confidence"][
            "finite_sample_joint_coverage"
        ],
        "order_stratum_medians": medians,
        "order_stratum_upper_bounds": upper_bounds,
        "regression_limit": protocol["regression_limit"],
        "passed": upper_confidence_bound <= protocol["regression_limit"],
    }


def _complete_transcripts(directory: pathlib.Path) -> list[dict[str, float]]:
    expected = {f"runtime-divan-{repetition:02d}.txt" for repetition in range(1, 11)}
    actual = {path.name for path in directory.glob("runtime-divan-*.txt")}
    if actual != expected:
        raise ValueError(f"runtime transcript set is incomplete in {directory}")
    return [
        validate_divan_transcript(directory / f"runtime-divan-{repetition:02d}.txt")
        for repetition in range(1, 11)
    ]




def _write_json(path: pathlib.Path, document: dict[str, Any]) -> None:
    if path.exists():
        raise ValueError(f"refusing to overwrite evidence: {path}")
    path.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")




def _command_validate_budget(arguments: argparse.Namespace) -> int:
    document = validate_budget_document(
        arguments.budget,
        arguments.expected_reference,
        arguments.candidate_root,
        arguments.reference_root,
    )
    print(
        json.dumps(
            {
                "reference_revision": document["measured_revision"],
                "runner": document["runner_cohort"],
                "toolchain": document["toolchain"],
                "runner_source_hash": document["runner_source_hash"],
                "budget_sha256": _sha256(arguments.budget),
                "rows": len(document["rows"]),
            },
            sort_keys=True,
        )
    )
    return 0


def _command_validate_allocation_manifest(arguments: argparse.Namespace) -> int:
    workloads = validate_allocation_manifest(arguments.manifest)
    print(
        json.dumps(
            {
                "allocation_manifest_sha256": _sha256(arguments.manifest),
                "rows": len(workloads),
            },
            sort_keys=True,
        )
    )
    return 0


def _command_validate_transcript(arguments: argparse.Namespace) -> int:
    rows = validate_divan_transcript(arguments.input, arguments.expected_rows)
    print(json.dumps({"rows": len(rows)}, sort_keys=True))
    return 0


def _command_validate_isolated_transcript(arguments: argparse.Namespace) -> int:
    rows = validate_isolated_transcript(arguments.input, arguments.scenario)
    print(json.dumps({"rows": len(rows), "scenario": arguments.scenario}, sort_keys=True))
    return 0


def _command_measurement_plan(arguments: argparse.Namespace) -> int:
    protocol = load_protocol(arguments.protocol)
    print(json.dumps(measurement_plan(protocol, arguments.mode), indent=2, sort_keys=True))
    return 0


def _command_parse_time(arguments: argparse.Namespace) -> int:
    try:
        counters = parse_darwin_time(arguments.input.read_text(encoding="utf-8"))
    except OSError as error:
        raise ValueError(f"cannot read Darwin time transcript: {arguments.input}") from error
    _write_json(arguments.output, counters)
    return 0






def _command_evaluate_protocol(arguments: argparse.Namespace) -> int:
    protocol = load_protocol(arguments.protocol)
    metadata = _load_protocol_metadata(arguments.metadata, protocol, arguments.plan)
    validate_protocol_sources(arguments.metadata.parent, metadata, protocol)
    subject, control, comparison = evaluate_protocol_paired(
        arguments.budget,
        protocol,
        arguments.subject,
        arguments.control,
        arguments.plan,
        metadata,
    )
    validate_protocol_diagnostics(
        arguments.diagnostics,
        (metadata["subject_role"], metadata["control_role"]),
        protocol,
    )
    outputs = {
        f"{metadata['subject_role']}.json": subject,
        f"{metadata['control_role']}.json": control,
        "comparison.json": comparison,
    }
    if arguments.absolute_raw is None:
        raise ValueError("qualification requires subject-arm absolute evidence")
    absolute = evaluate_absolute(
        arguments.budget,
        arguments.absolute_raw,
        metadata["subject_revision"],
        metadata["capture_script_sha256"],
        metadata["subject_lock_sha256"],
        arguments.allocation_manifest,
        timing_report=subject,
    )
    absolute["subject_revision"] = absolute.pop("candidate_revision")
    absolute["metadata"] = metadata
    absolute["reference_only"] = metadata["mode"] == "reference-reference-aa"
    outputs["absolute.json"] = absolute

    for name in outputs:
        if (arguments.output_directory / name).exists():
            raise ValueError(f"refusing to overwrite evidence: {name}")
    for name, document in outputs.items():
        _write_json(arguments.output_directory / name, document)
    passed = comparison["passed"] and absolute["passed"]
    return 0 if passed else 1




def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    validate_budget = commands.add_parser("validate-budget")
    validate_budget.add_argument("--budget", type=pathlib.Path, required=True)
    validate_budget.add_argument("--expected-reference", required=True)
    validate_budget.add_argument("--candidate-root", type=pathlib.Path, required=True)
    validate_budget.add_argument("--reference-root", type=pathlib.Path, required=True)
    validate_budget.set_defaults(handler=_command_validate_budget)

    validate_transcript = commands.add_parser("validate-transcript")
    validate_transcript.add_argument("--input", type=pathlib.Path, required=True)
    validate_transcript.add_argument("--expected-rows", type=int, default=EXPECTED_DIVAN_ROWS)
    validate_transcript.set_defaults(handler=_command_validate_transcript)

    validate_isolated = commands.add_parser("validate-isolated-transcript")
    validate_isolated.add_argument("--input", type=pathlib.Path, required=True)
    validate_isolated.add_argument("--scenario", required=True)
    validate_isolated.set_defaults(handler=_command_validate_isolated_transcript)

    validate_allocation = commands.add_parser("validate-allocation-manifest")
    validate_allocation.add_argument("--manifest", type=pathlib.Path, required=True)
    validate_allocation.set_defaults(handler=_command_validate_allocation_manifest)

    plan = commands.add_parser("measurement-plan")
    plan.add_argument("--protocol", type=pathlib.Path, required=True)
    plan.add_argument(
        "--mode",
        choices=("candidate-reference", "reference-reference-aa"),
        required=True,
    )
    plan.set_defaults(handler=_command_measurement_plan)

    parse_time = commands.add_parser("parse-time")
    parse_time.add_argument("--input", type=pathlib.Path, required=True)
    parse_time.add_argument("--output", type=pathlib.Path, required=True)
    parse_time.set_defaults(handler=_command_parse_time)



    evaluate_protocol = commands.add_parser("evaluate-protocol")
    evaluate_protocol.add_argument("--budget", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--protocol", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--subject", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--control", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--diagnostics", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--plan", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--metadata", type=pathlib.Path, required=True)
    evaluate_protocol.add_argument("--absolute-raw", type=pathlib.Path)
    evaluate_protocol.add_argument(
        "--allocation-manifest", type=pathlib.Path, required=True
    )
    evaluate_protocol.add_argument(
        "--output-directory", type=pathlib.Path, required=True
    )
    evaluate_protocol.set_defaults(handler=_command_evaluate_protocol)

    return parser


def main() -> int:
    try:
        arguments = _parser().parse_args()
        return arguments.handler(arguments)
    except ValueError as error:
        print(f"composition performance evidence invalid: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
