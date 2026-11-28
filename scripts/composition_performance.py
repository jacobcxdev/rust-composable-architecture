#!/usr/bin/env python3
"""Validate and compare RCA composition performance evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import ntpath
import pathlib
import re
import statistics
import sys
from typing import Any


BUDGET_SHA256 = "f96c9f8ed2ebcee6ff63b11750ed1fd02d9f88f5c60ff740a184071c8032a878"
RUNNER = "mac15-9-m3-max-16c-48gb-macos27plus-ac-high"
MINIMUM_MACOS_MAJOR = 27
RUST100_TOOL_VERSIONS = {
    "rustc": "rustc 1.100.0 (0ef1bc386 2026-11-09)",
    "cargo": "cargo 1.100.0 (3acc79c0c 2026-11-05)",
    "rustdoc": "rustdoc 1.100.0 (0ef1bc386 2026-11-09)",
}
RUST100_METHOD = "composition-paired-v9"
WINDOWS_V6_METHOD = "composition-windows-paired-v6"
TWO_LOOK_METHOD = "two-look-seven-twelve-v2"
RUST100_PROTOCOL_SHA256 = "432ab7f75a41c288f2c3f9ccb79fc39199c0942b449bd862a20ae04e48efa37b"
WINDOWS_V6_PROTOCOL_SHA256 = "407231c0d126a0e5e32caf580ba40e5f78fe0cd5465e68f34bd5f166f92e51ca"
REGRESSION_LIMIT = 1.10
EXPECTED_DIVAN_ROWS = 153
EXPECTED_ALLOCATIONS = 47
ALLOCATION_MANIFEST_SHA256 = (
    "95a9e2ee87deaa3046d4a51893efe164282d51aef6a3b3eec61e9f1ca0b3bf32"
)
WINDOWS_RUNNER = "ryzen9-3900x-windows-x86_64-msvc"
WINDOWS_COMPILER_TELEMETRY = {
    "path_suffix": r"VC\Tools\MSVC\14.51.36231\bin\Hostx64\x64\VCTIP.EXE",
    "sha256": "41fe46f8a0b41f6dd94c68c85fba20a10aacb662a9ef04784a632e37f3692371",
}
WINDOWS_V5_ISOLATED_DIVAN_ARGUMENTS = ["--min-time", "2", "--max-time", "2"]
WINDOWS_PRIMARY_THREAD_AFFINITY = {
    "mask": 1, "group": 0, "apply_before_resume": True,
    "process_affinity": "unchanged-full-system", "commands": "normal-divan-timing-only",
}
WINDOWS_PREFIX_REJECTION = {
    "criterion": "irrecoverable-final-upper-bound-after-sealed-mode-block",
    "minimum_observations_per_order_stratum": 2,
    "successful_capture_requires_all_blocks_and_gates": True,
}
_INTERLEAVING = {
    "policy": "alternating-mode-first-within-block",
    "blocks": 20,
    "odd_block_modes": ["reference-reference-aa", "candidate-reference"],
    "even_block_modes": ["candidate-reference", "reference-reference-aa"],
    "acceptance": "both modes must pass in the same authenticated capture",
}
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


def is_windows_protocol(protocol: dict[str, Any]) -> bool:
    """Identify the Windows method without treating it as macOS absolute evidence."""
    return protocol.get("method_version") == WINDOWS_V6_METHOD


def is_current_mac_protocol(protocol: dict[str, Any]) -> bool:
    return protocol.get("method_version") == RUST100_METHOD


def is_current_protocol(protocol: dict[str, Any]) -> bool:
    return is_current_mac_protocol(protocol) or is_windows_protocol(protocol)


def expected_tool_versions(protocol: dict[str, Any]) -> dict[str, str]:
    """Return the tools required by a supported qualification protocol."""
    if not is_current_protocol(protocol):
        raise ValueError("unsupported composition protocol method")
    return dict(RUST100_TOOL_VERSIONS)


def expected_reference_revision(protocol: dict[str, Any]) -> str:
    """Resolve the admitted reference when checking a measurement."""
    return _reference_identity(protocol)["revision"]


def _reference_identity(protocol: dict[str, Any]) -> dict[str, str]:
    if not is_current_protocol(protocol):
        raise ValueError("unsupported composition protocol method")
    from native_release_reference import composition_reference

    return composition_reference()


def expected_toolchain(protocol: dict[str, Any]) -> str:
    versions = expected_tool_versions(protocol)
    return versions["rustc"] + "; " + versions["cargo"]


def is_interleaved_protocol(protocol: dict[str, Any]) -> bool:
    """Both supported methods measure both comparison modes in one capture."""
    return is_current_protocol(protocol)


def _protocol_sha256(protocol: dict[str, Any]) -> str:
    if is_current_mac_protocol(protocol):
        return RUST100_PROTOCOL_SHA256
    if is_windows_protocol(protocol):
        return WINDOWS_V6_PROTOCOL_SHA256
    raise ValueError("unsupported composition protocol method")


def load_protocol(path: pathlib.Path) -> dict[str, Any]:
    """Load and validate the frozen composition qualification protocol."""
    try:
        contents = path.read_bytes()
        document = json.loads(contents)
    except (OSError, ValueError) as error:
        raise ValueError(f"cannot read composition protocol: {error}") from error
    if not isinstance(document, dict):
        raise ValueError("composition protocol must be an object")
    if hashlib.sha256(contents).hexdigest() != _protocol_sha256(document):
        raise ValueError("composition protocol file hash is invalid")
    required = {
        "schema_version": 1,
        "status": "frozen-before-measurement",
        "runner": WINDOWS_RUNNER if is_windows_protocol(document) else RUNNER,
        "reference_revision": "published-stable-release",
        "budget_sha256": BUDGET_SHA256,
        "allocation_manifest_sha256": ALLOCATION_MANIFEST_SHA256,
        "regression_limit": REGRESSION_LIMIT,
    }
    if is_windows_protocol(document):
        required.update(platform="windows", target="x86_64-pc-windows-msvc", scope="paired-only")
        if (document.get("absolute_compatibility", {}).get("qualified") is not False
                or document.get("absolute_compatibility", {}).get("budget_changes_permitted") is not False
                or "minimum_macos_major" in document):
            raise ValueError("Windows protocol cannot qualify macOS absolute budgets")
    else:
        required["minimum_macos_major"] = MINIMUM_MACOS_MAJOR
    required["toolchain"] = expected_toolchain(document)
    if is_current_mac_protocol(document):
        required["scope"] = "paired-and-macos-absolute"
    if is_windows_protocol(document):
        required["process_memory_limit_bytes"] = 2 * 1024**3
        required["compiler_telemetry"] = WINDOWS_COMPILER_TELEMETRY
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
    if is_windows_protocol(document):
        if runtime.get("isolated_divan_arguments") != WINDOWS_V5_ISOLATED_DIVAN_ARGUMENTS:
            raise ValueError("Windows isolated sampling arguments differ from the frozen policy")
        if "isolated_divan_arguments_by_scenario" in runtime:
            raise ValueError("Windows sampling arguments cannot have scenario overrides")
        placement = runtime.get("primary_thread_affinity")
        if (placement != WINDOWS_PRIMARY_THREAD_AFFINITY
                or type(placement.get("mask")) is not int or type(placement.get("group")) is not int
                or placement.get("apply_before_resume") is not True):
            raise ValueError("Windows primary-thread affinity differs from the frozen policy")
        if document.get("early_rejection") != WINDOWS_PREFIX_REJECTION:
            raise ValueError("Windows prefix rejection differs from the frozen policy")
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
    expected_quiescence = {}
    if not is_windows_protocol(document):
        expected_quiescence.update(
            process_monitor_interval_seconds=1,
            thermal_policy="nsprocessinfo-nominal-and-pmset-no-recorded-warning")
    expected_quiescence.update(admission="recorded-background-load", consecutive_samples=1)
    interleaved_mode_order(document, 1)
    for key, expected in expected_quiescence.items():
        if quiescence.get(key) != expected:
            raise ValueError(f"composition protocol quiescence mismatch: {key}")
    return document


def interleaved_mode_order(protocol: dict[str, Any], block: int, *, two_look: bool = False) -> tuple[str, str]:
    """Return the frozen mode order, including the optional four-block extension."""
    if (not is_interleaved_protocol(protocol)
            or protocol.get("interleaving") != _INTERLEAVING):
        raise ValueError("composition interleaving policy is invalid")
    last = 24 if two_look and protocol.get("method_version") == WINDOWS_V6_METHOD else 20
    if type(block) is not int or not 1 <= block <= last:
        raise ValueError(f"composition interleaving block must be between 1 and {last}")
    modes = _INTERLEAVING["odd_block_modes" if block % 2 else "even_block_modes"]
    return tuple(modes)


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


def timing_schedule(protocol: dict[str, Any], *, two_look: bool = False) -> list[str]:
    """Return the original orders or the balanced first look and four extra orders."""
    schedule = seeded_order_schedule(protocol)
    if two_look:
        if protocol.get("method_version") != WINDOWS_V6_METHOD:
            raise ValueError("the two-look extension requires Windows v6")
        # Swap physical slots, never observations. The first fourteen now have
        # seven of each order, and the first twenty still have ten of each.
        schedule[13], schedule[14] = schedule[14], schedule[13]
        schedule.extend(("control-subject", "subject-control", "subject-control", "control-subject"))
    return schedule


def measurement_plan(protocol: dict[str, Any], mode: str, *, two_look: bool = False) -> dict[str, Any]:
    """Expand the frozen protocol into its result-independent measurement plan."""
    if mode == "candidate-reference":
        semantic_roles = {"subject": "candidate", "control": "reference"}
    elif mode == "reference-reference-aa":
        semantic_roles = {"subject": "reference-b", "control": "reference-a"}
    else:
        raise ValueError(f"unsupported paired measurement mode: {mode}")

    runtime = []
    compile_rows = []
    for block, order in enumerate(timing_schedule(protocol, two_look=two_look), start=1):
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
    plan = {
        "schema_version": 1,
        "method_version": protocol["method_version"],
        "protocol_sha256": _protocol_sha256(protocol),
        "mode": mode,
        "seed": protocol["randomisation"]["seed"],
        "regression_limit": protocol["regression_limit"],
        "confidence": protocol["confidence"],
        "quiescence": protocol["quiescence"],
        "resource_counters": protocol["resource_counters"],
        "runtime": runtime,
        "compile": compile_rows,
    }
    if two_look:
        plan["timing_method"] = TWO_LOOK_METHOD
    if is_interleaved_protocol(protocol):
        interleaved_mode_order(protocol, 1)
        plan["interleaving"] = protocol["interleaving"]
    if is_current_protocol(protocol):
        plan["toolchain"] = expected_toolchain(protocol)
    if is_windows_protocol(protocol):
        plan["process_memory_limit_bytes"] = protocol["process_memory_limit_bytes"]
    return plan


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


def find_competing_processes(
    snapshot: str, own_pid: int, *, process_prefixes: tuple[str, ...] = ("composition-",)
) -> list[dict[str, Any]]:
    """Report compilers and selected benchmark processes outside our process tree."""
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
            basename in blocked_names or basename.startswith(process_prefixes)
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
    """Record background load. Reject thermal warnings and other measurement runs."""
    policy = protocol["quiescence"]
    if any(type(value) not in (int, float) or not math.isfinite(value) or value < 0
           for value in (cpu_used_percent, load_average_1m)) or cpu_used_percent > 100:
        raise ValueError("invalid CPU or load telemetry")
    if not is_current_mac_protocol(protocol) or policy.get("admission") != "recorded-background-load":
        raise ValueError("composition telemetry admission policy is invalid")
    required_thermal = {
        "Note: No thermal warning level has been recorded",
        "Note: No performance warning level has been recorded",
        "Note: No CPU power status has been recorded",
    }
    if not required_thermal.issubset(set(thermal_text.splitlines())):
        raise ValueError("host thermal warning-history proxy is not canonical")
    process_prefixes = ("composition-", "qualification-")
    competitors = find_competing_processes(
        process_snapshot, own_pid, process_prefixes=process_prefixes)
    if any(
        pathlib.PurePath(row["command"]).name.startswith(process_prefixes)
        for row in competitors
    ):
        raise ValueError("another composition or qualification measurement is running")
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
    path: pathlib.Path, protocol: dict[str, Any], *, command_kind: str = "runtime", rank_cold: bool = False,
) -> dict[str, Any]:
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read per-process resource counters: {path}") from error
    required = protocol["resource_counters"]["required"]
    if not isinstance(document, dict) or set(document) != set(required):
        raise ValueError(f"per-process resource counter set is invalid: {path}")
    if is_windows_protocol(protocol):
        validate_windows_resource_counters(document, expected_kind=command_kind,
            rank_cold=rank_cold,
            primary_thread_affinity_required=(protocol["method_version"] == WINDOWS_V6_METHOD
                                              and command_kind == "runtime"))
        return document
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


def _windows_telemetry_path(value: Any) -> str:
    """Require the canonical, fully qualified path to the supported MSVC tool."""
    if not isinstance(value, str):
        raise ValueError("missing Windows compiler telemetry path")
    image = pathlib.PureWindowsPath(value)
    if (not image.is_absolute() or ntpath.normpath(value) != value
            or not image.match(WINDOWS_COMPILER_TELEMETRY["path_suffix"])):
        raise ValueError("Windows compiler telemetry path differs from the frozen protocol")
    return value


def validate_windows_resource_counters(
    document: dict[str, Any], *, expected_kind: str | None = None,
    primary_thread_affinity_required: bool = False, rank_cold: bool = False,
) -> None:
    """Keep command timing, owned cleanup and direct-process counters separate."""
    if not isinstance(document, dict) or set(document) != {"real_seconds", "direct_process", "process_receipt"}:
        raise ValueError("Windows resource counter set is invalid")
    elapsed = document["real_seconds"]
    if type(elapsed) not in (float, int) or not math.isfinite(elapsed) or elapsed <= 0:
        raise ValueError("Windows elapsed time must be positive and finite")
    counters = document["direct_process"]
    positive = {"QueryProcessCycleTime", "CreationTime100ns", "ExitTime100ns",
                "PeakWorkingSetSize", "PeakPagefileUsage"}
    nonnegative = {"KernelTime100ns", "UserTime100ns", "PageFaultCount"}
    if (not isinstance(counters, dict)
            or set(counters) != positive | nonnegative | {"instructions_retired"}):
        raise ValueError("Windows direct-process counter set is invalid")
    for field in positive | nonnegative:
        value = counters[field]
        if type(value) is not int or value < (1 if field in positive else 0):
            raise ValueError(f"Windows direct-process counter is invalid: {field}")
    if (counters["ExitTime100ns"] < counters["CreationTime100ns"]
            or counters["instructions_retired"] is not None):
        raise ValueError("Windows process lifetime or unsupported instruction counter is invalid")

    receipt = document["process_receipt"]
    fields = {"command_kind", "returncode", "elapsed_seconds", "leader_elapsed_seconds",
              "descendants_exited", "descendants_exited_naturally", "compiler_telemetry_path",
              "compiler_telemetry_sha256", "terminated_telemetry", "job_memory_limit_bytes"}
    if rank_cold:
        fields.add("build_dir")
        if (expected_kind != "compile" or not isinstance(receipt, dict)
                or not isinstance(receipt.get("build_dir"), str) or not ntpath.isabs(receipt["build_dir"])):
            raise ValueError("rank cold build directory is missing or invalid")
    if isinstance(receipt, dict) and "divan_arguments" in receipt:
        fields.add("divan_arguments")
        if (receipt.get("command_kind") != "runtime"
                or not isinstance(receipt["divan_arguments"], list)
                or not all(isinstance(value, str) for value in receipt["divan_arguments"])):
            raise ValueError("Windows isolated sampling arguments are invalid")
    if primary_thread_affinity_required:
        fields.add("primary_thread_affinity")
        placement = receipt.get("primary_thread_affinity") if isinstance(receipt, dict) else None
        expected_fields = {"mask", "previous_mask", "group", "process_mask", "system_mask", "applied_before_resume"}
        if (not isinstance(placement, dict) or set(placement) != expected_fields
                or any(type(placement[key]) is not int for key in expected_fields - {"applied_before_resume"})
                or placement["mask"] != 1 or placement["group"] != 0
                or not 0 < placement["system_mask"] < 2**64 or not (placement["system_mask"] & 1)
                or placement["previous_mask"] != placement["system_mask"]
                or placement["process_mask"] != placement["system_mask"]
                or placement["applied_before_resume"] is not True
                or receipt.get("command_kind") != "runtime" or "divan_arguments" not in receipt):
            raise ValueError("Windows primary-thread affinity receipt is missing or invalid")
    elif isinstance(receipt, dict) and "primary_thread_affinity" in receipt:
        raise ValueError("Windows primary-thread affinity is not part of this measurement")
    if not isinstance(receipt, dict) or set(receipt) != fields:
        raise ValueError("Windows process receipt fields are invalid")
    kind = receipt["command_kind"]
    if kind not in ("compile", "runtime") or (expected_kind is not None and kind != expected_kind):
        raise ValueError("Windows process receipt command kind differs")
    if (type(receipt["returncode"]) is not int or receipt["returncode"] != 0
            or receipt["descendants_exited"] is not True
            or type(receipt["descendants_exited_naturally"]) is not bool
            or type(receipt["job_memory_limit_bytes"]) is not int
            or receipt["job_memory_limit_bytes"] != 2 * 1024**3):
        raise ValueError("Windows process receipt does not prove successful owned cleanup")
    for field in ("elapsed_seconds", "leader_elapsed_seconds"):
        value = receipt[field]
        if type(value) not in (float, int) or not math.isfinite(value) or value <= 0:
            raise ValueError("Windows process receipt elapsed time is invalid")
    if receipt["leader_elapsed_seconds"] > receipt["elapsed_seconds"]:
        raise ValueError("Windows process receipt cleanup precedes the leader exit")
    elapsed_field = "leader_elapsed_seconds" if kind == "compile" else "elapsed_seconds"
    if elapsed != receipt[elapsed_field]:
        raise ValueError("Windows measurement uses the wrong elapsed-time boundary")

    terminated = receipt["terminated_telemetry"]
    if not isinstance(terminated, list) or receipt["descendants_exited_naturally"] != (not terminated):
        raise ValueError("Windows process receipt natural-exit claim differs")
    if kind == "runtime":
        if (terminated or receipt["compiler_telemetry_path"] is not None
                or receipt["compiler_telemetry_sha256"] is not None):
            raise ValueError("Windows runtime measurement cannot terminate compiler telemetry")
        return
    telemetry_path = _windows_telemetry_path(receipt["compiler_telemetry_path"])
    if receipt["compiler_telemetry_sha256"] != WINDOWS_COMPILER_TELEMETRY["sha256"]:
        raise ValueError("Windows compiler telemetry identity differs from the frozen protocol")
    seen = set()
    for process in terminated:
        if (not isinstance(process, dict) or set(process) != {"pid", "image_path"}
                or type(process["pid"]) is not int or process["pid"] <= 0
                or process["pid"] in seen or not isinstance(process["image_path"], str)
                or ntpath.normpath(process["image_path"]) != process["image_path"]
                or ntpath.normcase(process["image_path"]) != ntpath.normcase(telemetry_path)):
            raise ValueError("Windows terminated telemetry process is not the pinned compiler descendant")
        seen.add(process["pid"])


def validate_mac_memory_limit(value: int) -> int:
    """Require one whole-GiB ceiling without increasing the eight-GiB maximum."""
    if type(value) is not int or not 1024**3 <= value <= 8 * 1024**3 or value % 1024**3:
        raise ValueError("Mac memory watchdog ceiling must be one through eight whole GiB")
    return value


def validate_memory_watchdog_receipt(
    transcript: pathlib.Path, expected_sha256: str | None, memory_limit_bytes: int
) -> None:
    """Reject measurements without successful, matching memory protection records."""
    if not isinstance(expected_sha256, str) or not _HASH.fullmatch(expected_sha256):
        raise ValueError("memory watchdog source hash is invalid")
    validate_mac_memory_limit(memory_limit_bytes)
    if not transcript.is_file():
        raise ValueError(f"memory watchdog command transcript is missing: {transcript}")
    try:
        receipt = json.loads(transcript.with_suffix(".memory.json").read_text(encoding="utf-8"))
        exit_record = json.loads(transcript.with_suffix(".exit.json").read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read memory watchdog receipts: {transcript}") from error
    required = {
        "protection": "polling-watchdog-not-kernel-memory-cap",
        "started": True,
        "reason": "command-exited",
        "returncode": 0,
        "command_returncode": 0,
        "cleanup_complete": True,
        "observer_complete": True,
        "descendants_complete": True,
        "forced_cleanup": False,
        "memory_limit_bytes": memory_limit_bytes,
        "host_reserve_bytes": 4294967296,
        "sample_interval_seconds": 0.25,
        "memory_watchdog_sha256": expected_sha256,
    }
    if not isinstance(receipt, dict) or any(
        type(receipt.get(field)) is not type(value) or receipt[field] != value
        for field, value in required.items()
    ):
        raise ValueError(f"memory watchdog did not prove successful protection: {transcript}")
    if any(type(receipt.get(field)) is not int or receipt[field] < 0
           for field in ("peak_footprint_bytes", "peak_process_count")):
        raise ValueError(f"memory watchdog peak measurement is invalid: {transcript}")
    if receipt["peak_footprint_bytes"] >= receipt["memory_limit_bytes"]:
        raise ValueError(f"memory watchdog peak reached its stop threshold: {transcript}")
    if (not isinstance(exit_record, dict) or type(exit_record.get("returncode")) is not int
            or exit_record["returncode"] != 0):
        raise ValueError(f"memory watchdog command exit is not successful: {transcript}")


def _measurement_suffixes(protocol: dict[str, Any]) -> tuple[str, ...]:
    """Include the watchdog receipts for a macOS measurement."""
    suffixes = (".txt", ".resources.json")
    if is_current_mac_protocol(protocol):
        return (*suffixes, ".memory.json", ".exit.json")
    return suffixes


def isolated_divan_arguments(protocol: dict[str, Any], scenario: str, *, experimental_rank: bool = False) -> list[str]:
    """Return the frozen duration arguments for one Windows runtime scenario."""
    if not is_windows_protocol(protocol) or scenario not in protocol["runtime"]["gated_scenarios"]:
        raise ValueError("Windows runtime scenario is not frozen in the protocol")
    if experimental_rank and protocol["method_version"] != WINDOWS_V6_METHOD:
        raise ValueError("experimental rank windows require the frozen Windows v6 protocol")
    runtime = protocol["runtime"]
    return list(runtime.get("isolated_divan_arguments_by_scenario", {}).get(
        scenario, runtime["isolated_divan_arguments"]))


def load_isolated_runtime_pairs(
    subject_directory: pathlib.Path,
    control_directory: pathlib.Path,
    scenario: str,
    protocol: dict[str, Any],
    memory_watchdog_sha256: str | None = None,
    memory_limit_bytes: int | None = None,
    *, block_count: int | None = None, experimental_rank: bool = False, two_look: bool = False,
) -> list[dict[str, Any]]:
    """Load one isolated observation per role for every seeded block."""
    if scenario not in protocol["runtime"]["gated_scenarios"]:
        raise ValueError(f"runtime scenario is not frozen in the protocol: {scenario}")
    schedule = _paired_schedule(protocol, block_count, two_look=two_look)
    slug = scenario.replace("/", "__")
    expected_names = {
        f"block-{block:02d}{suffix}"
        for block in range(1, len(schedule) + 1)
        for suffix in _measurement_suffixes(protocol)
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
        if is_current_mac_protocol(protocol):
            for transcript in (subject_path, control_path):
                validate_memory_watchdog_receipt(transcript, memory_watchdog_sha256, memory_limit_bytes)
        subject = validate_isolated_transcript(subject_path, scenario)[scenario]
        control = validate_isolated_transcript(control_path, scenario)[scenario]
        subject_resources = _load_resource_counters(
            subject_path.with_suffix(".resources.json"), protocol
        )
        control_resources = _load_resource_counters(
            control_path.with_suffix(".resources.json"), protocol
        )
        if is_windows_protocol(protocol):
            expected_arguments = ["--bench", "--exact", "composition::" + scenario.replace("/", "::"),
                                  *isolated_divan_arguments(protocol, scenario, experimental_rank=experimental_rank)]
            for resources in (subject_resources, control_resources):
                if resources["process_receipt"].get("divan_arguments") != expected_arguments:
                    raise ValueError("Windows isolated sampling arguments differ from the frozen policy")
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
    memory_watchdog_sha256: str | None = None,
    memory_limit_bytes: int | None = None,
    *, block_count: int | None = None, experimental_rank: bool = False, two_look: bool = False,
) -> list[dict[str, Any]]:
    """Load balanced, clean compile observations from isolated targets."""
    if fixture not in protocol["compile"]["fixtures"]:
        raise ValueError(f"compile fixture is not frozen in the protocol: {fixture}")
    schedule = _paired_schedule(protocol, block_count, two_look=two_look)
    expected_names = {
        f"block-{block:02d}{suffix}"
        for block in range(1, len(schedule) + 1)
        for suffix in _measurement_suffixes(protocol)
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
        if is_current_mac_protocol(protocol):
            for transcript in (subject_path, control_path):
                validate_memory_watchdog_receipt(transcript, memory_watchdog_sha256, memory_limit_bytes)
        if not subject_path.read_text(encoding="utf-8").strip() or not control_path.read_text(
            encoding="utf-8"
        ).strip():
            raise ValueError(f"paired compile transcript is empty: {fixture} block {block}")
        subject_resources = _load_resource_counters(
            subject_path.with_suffix(".resources.json"), protocol, command_kind="compile", rank_cold=experimental_rank
        )
        control_resources = _load_resource_counters(
            control_path.with_suffix(".resources.json"), protocol, command_kind="compile", rank_cold=experimental_rank
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
    path: pathlib.Path, protocol: dict[str, Any], mode: str, *, two_look: bool = False
) -> dict[str, Any]:
    try:
        plan = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read frozen measurement plan: {path}") from error
    expected = measurement_plan(protocol, mode, two_look=two_look)
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


def _validate_protocol_pair_metadata(
    metadata: dict[str, Any], method_version: str | None = None
) -> None:
    version = method_version or metadata.get("method_version")
    if is_windows_protocol({"method_version": version}):
        if (metadata.get("platform") != "windows"
                or metadata.get("target") != "x86_64-pc-windows-msvc"
                or not re.fullmatch(r"10\.[0-9]+(?:\.[0-9]+){1,2}", str(metadata.get("os_version", "")))):
            raise ValueError("paired evidence requires its Windows platform and target")
    else:
        if metadata.get("platform") not in (None, "macos"):
            raise ValueError("macOS paired evidence cannot use another platform")
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
    identity = _reference_identity({"method_version": version})
    reference = identity["revision"]
    reference_kind = identity["mode"]
    if reference_kind not in ("first-release-enrolment", "release-comparison") or metadata.get("reference_kind") != reference_kind:
        raise ValueError("paired evidence reference kind differs from the admitted reference")
    if metadata["control_revision"] != reference:
        raise ValueError("paired control revision is not the admitted reference")
    if reference_kind == "first-release-enrolment":
        if metadata["subject_revision"] != reference:
            raise ValueError("first-release enrolment requires the signed source in both arms")
    elif mode == "candidate-reference":
        if metadata["subject_revision"] == reference:
            raise ValueError("candidate and reference revisions must differ")
    elif metadata["subject_revision"] != reference:
        raise ValueError("A/A evidence must use the frozen reference twice")
    capture_hash = metadata.get("capture_sha256")
    if not isinstance(capture_hash, str) or not _HASH.fullmatch(capture_hash):
        raise ValueError("paired evidence capture hash is invalid")
    if is_current_mac_protocol({"method_version": version}):
        validate_mac_memory_limit(metadata.get("memory_limit_bytes"))
        watchdog_hash = metadata.get("memory_watchdog_sha256")
        if not isinstance(watchdog_hash, str) or not _HASH.fullmatch(watchdog_hash):
            raise ValueError("paired evidence memory watchdog hash is invalid")


def evaluate_protocol_paired(
    budget_path: pathlib.Path,
    protocol: dict[str, Any],
    subject_directory: pathlib.Path,
    control_directory: pathlib.Path,
    plan_path: pathlib.Path,
    metadata: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Any], dict[str, Any]]:
    """Evaluate isolated runtime and contemporaneous clean-compile pairs."""
    _validate_protocol_pair_metadata(metadata, protocol["method_version"])
    _validate_measurement_plan(plan_path, protocol, metadata["mode"])
    if is_current_mac_protocol(protocol):
        for role in ("subject", "control"):
            for suffix in ("", "-list"):
                validate_memory_watchdog_receipt(
                    plan_path.parent / "builds" / f"{role}{suffix}.txt",
                    metadata["memory_watchdog_sha256"],
                    metadata["memory_limit_bytes"],
                )
    document = validate_budget_document(budget_path)

    subject_rows = []
    control_rows = []
    comparisons = []
    loaded_runtime: dict[str, list[dict[str, Any]]] = {}
    for budget_row in _runtime_rows(document):
        scenario = budget_row["scenario"].removeprefix("runtime/")
        if scenario not in loaded_runtime:
            loaded_runtime[scenario] = load_isolated_runtime_pairs(
                subject_directory, control_directory, scenario, protocol,
                metadata.get("memory_watchdog_sha256"),
                metadata.get("memory_limit_bytes"),
            )
        pairs = loaded_runtime[scenario]
        direction = _direction(budget_row)
        converted = []
        for pair in pairs:
            converted.append(
                {
                    **pair,
                    "subject": _runtime_value(budget_row, {scenario: pair["subject"]}),
                    "control": _runtime_value(budget_row, {scenario: pair["control"]}),
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
    telemetry = (_windows_telemetry_path(metadata.get("compiler_telemetry_path"))
                 if is_windows_protocol(protocol) else None)
    for fixture in protocol["compile"]["fixtures"]:
        budget_row = compile_budgets[fixture]
        pairs = load_compile_pairs(
            subject_directory, control_directory, fixture, protocol,
            metadata.get("memory_watchdog_sha256"),
            metadata.get("memory_limit_bytes"),
        )
        if telemetry is not None and any(
            ntpath.normcase(pair[f"{role}_resources"]["process_receipt"]["compiler_telemetry_path"]) != ntpath.normcase(telemetry)
            for pair in pairs for role in ("subject", "control")
        ):
            raise ValueError("Windows compile receipt used a different compiler telemetry installation")
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
        "reference_kind": metadata["reference_kind"],
        "mode": metadata["mode"],
        "runner": protocol["runner"],
        "toolchain": expected_toolchain(protocol),
        "method_version": protocol["method_version"],
        "protocol_sha256": _protocol_sha256(protocol),
        "metadata": metadata,
    }
    if is_windows_protocol(protocol):
        common["qualification_scope"] = "paired-only"
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
        "prior_release_comparison": metadata["mode"] == "candidate-reference" and metadata["reference_kind"] == "release-comparison",
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
    memory_watchdog_sha256: str | None = None,
    memory_limit_bytes: int | None = None,
) -> None:
    expected = {
        f"{role}{suffix}"
        for role in roles
        for suffix in _measurement_suffixes(protocol)
    }
    actual = {path.name for path in directory.iterdir()} if directory.is_dir() else set()
    if actual != expected:
        raise ValueError("full Divan diagnostic evidence set is incomplete")
    for role in roles:
        transcript = directory / f"{role}.txt"
        if is_current_mac_protocol(protocol):
            validate_memory_watchdog_receipt(transcript, memory_watchdog_sha256, memory_limit_bytes)
        validate_divan_transcript(
            transcript, protocol["runtime"]["diagnostic_timing_rows"]
        )
        resources = _load_resource_counters(transcript.with_suffix(".resources.json"), protocol)
        if (protocol.get("method_version") == WINDOWS_V6_METHOD
                and resources["process_receipt"].get("divan_arguments") != ["--bench"]):
            raise ValueError("Windows diagnostic sampling arguments differ from the frozen policy")


def _load_protocol_metadata(
    path: pathlib.Path, protocol: dict[str, Any], plan_path: pathlib.Path
) -> dict[str, Any]:
    try:
        metadata = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read paired protocol metadata: {path}") from error
    if not isinstance(metadata, dict):
        raise ValueError("paired protocol metadata must be an object")
    _validate_protocol_pair_metadata(metadata, protocol["method_version"])
    if is_windows_protocol(protocol):
        try:
            inputs = json.loads((path.parent / "inputs.json").read_bytes())
            enrolled = inputs["tools"]["compiler_telemetry"]
            pinned = inputs["input_hashes"][enrolled["path"]]
        except (OSError, ValueError, TypeError, KeyError) as error:
            raise ValueError("missing Windows compiler telemetry enrolment") from error
        telemetry = _windows_telemetry_path(metadata.get("compiler_telemetry_path"))
        enrolled_path = _windows_telemetry_path(enrolled["path"])
        if (ntpath.normcase(telemetry) != ntpath.normcase(enrolled_path)
                or enrolled.get("sha256") != WINDOWS_COMPILER_TELEMETRY["sha256"]
                or pinned != enrolled["sha256"]):
            raise ValueError("Windows compiler telemetry does not match the premeasurement inputs")
    expected = {
        "schema_version": 1,
        "method_version": protocol["method_version"],
        "protocol_sha256": _protocol_sha256(protocol),
        "budget_sha256": BUDGET_SHA256,
        "allocation_manifest_sha256": ALLOCATION_MANIFEST_SHA256,
        "seed": protocol["randomisation"]["seed"],
        "runner": protocol["runner"],
        "toolchain": expected_toolchain(protocol),
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
        pathlib.Path(__file__).resolve().parents[1] / "benches/performance/composition-budgets.json")["rows"]}
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
        if receipt.get("adapters") != ["external-zero-host-v1"]:
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
            if receipt["effective"].get(name) != row["sha256"]:
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


def validate_budget_document(path: pathlib.Path) -> dict[str, Any]:
    """Check the 33 release limits against the reviewed policy, not a previous capture."""
    try:
        contents = path.read_bytes()
        document = json.loads(contents)
    except (OSError, ValueError) as error:
        raise ValueError(f"cannot read composition budgets: {error}") from error
    if not isinstance(document, dict) or document.keys() != {"schema_version", "rows"} or document["schema_version"] != 3:
        raise ValueError("invalid composition budget schema")

    rows = document["rows"]
    if not isinstance(rows, list) or len(rows) != 33:
        raise ValueError("composition budgets must contain exactly 33 rows")
    categories = {"runtime": 0, "allocation": 0, "compile": 0, "binary": 0}
    identities = set()
    for row in rows:
        if not isinstance(row, dict) or row.keys() != {"scenario", "measured_statistic", "units", "workload_source", "absolute_budget"}:
            raise ValueError("invalid composition budget row")
        if not all(isinstance(row[key], str) and row[key] for key in ("scenario", "measured_statistic", "units", "workload_source")):
            raise ValueError("invalid composition budget identity")
        scenario = row["scenario"]
        identity = (scenario, row["measured_statistic"])
        if identity in identities:
            raise ValueError(f"duplicate composition budget row: {scenario}")
        identities.add(identity)
        category = scenario.partition("/")[0]
        if category not in categories:
            raise ValueError(f"unsupported composition budget category: {category}")
        categories[category] += 1
        value = row["absolute_budget"]
        source = pathlib.PurePosixPath(row["workload_source"])
        if type(value) not in (int, float) or not math.isfinite(value) or value < 0 or source.is_absolute() or ".." in source.parts:
            raise ValueError(f"invalid composition budget: {scenario}")
    if categories != {"runtime": 10, "allocation": 13, "compile": 4, "binary": 6}:
        raise ValueError(f"composition budget category counts are invalid: {categories}")
    if hashlib.sha256(contents).hexdigest() != BUDGET_SHA256:
        raise ValueError("composition budget file hash is invalid")
    return document


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
    paired_host: dict[str, Any],
    timing_report: dict[str, Any],
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
        "cargo_version": expected_tool_versions(timing_report)["cargo"],
        "cargo_path": "<toolchain>/cargo",
        "rustc_path": "<toolchain>/rustc",
        "rustdoc_path": "<toolchain>/rustdoc",
        "rustc_wrapper": "disabled",
        "rustc_workspace_wrapper": "disabled",
        "bench_profile_override": "lto=false,debug=false,strip=true",
        "runtime_process_repetitions": "10",
        "runtime_expected_timing_rows": "153",
    }
    required.update(rustc_version=expected_tool_versions(timing_report)["rustc"],
                    rustdoc_version=expected_tool_versions(timing_report)["rustdoc"])
    for key, expected in required.items():
        if environment.get(key) != expected:
            raise ValueError(f"absolute capture environment mismatch: {key}")
    validate_measurement_os(environment.get("os_version", ""))
    for key in ("os_version", "os_build"):
        if environment.get(key) != paired_host.get(key):
            raise ValueError(f"absolute capture differs from paired host: {key}")
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


def _validate_zero_test(log, name):
    """Require one exact success from the maintained runner or ordinary libtest."""
    if not log.is_file():
        raise ValueError(f"missing zero-allocation evidence: {log.name}")
    text = log.read_text(encoding="utf-8")
    selected = re.findall(r"^test (\S+) \.\.\. (\S+)$", text, re.MULTILINE)
    summaries = [line for line in text.splitlines() if line.startswith("test result:")]
    summary = re.fullmatch(
        r"test result: (\w+)\. (\d+) passed; (\d+) failed"
        r"(?:; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in [^\r\n]+)?",
        summaries[0],
    ) if len(summaries) == 1 else None
    if (selected != [(name, "ok")] or summary is None
            or summary.groups()[:3] != ("ok", "1", "0")
            or summary.groups()[3:5] not in ((None, None), ("0", "0"))):
        raise ValueError(f"zero-allocation test did not pass exactly: {name}")
    filtered = summary.group(6)
    return {"test": name, "passed": 1, "failed": 0,
            "filtered_out": int(filtered) if filtered is not None else None,
            "stdout_sha256": _sha256(log)}


def _parse_binaries(path: pathlib.Path) -> dict[str, dict[str, Any]]:
    """Read the six native Mach-O products after stripping."""
    if not path.is_file():
        raise ValueError(f"missing binary evidence: {path.name}")
    rows: dict[str, dict[str, Any]] = {}
    current = None
    for line in path.read_text(encoding="utf-8").splitlines():
        field, separator, value = line.partition("=")
        if not separator or not value:
            raise ValueError(f"invalid binary evidence: {line}")
        if field == "binary":
            if value in rows:
                raise ValueError(f"duplicate binary evidence: {value}")
            current = rows[value] = {}
        elif current is None or field in current:
            raise ValueError(f"missing or duplicate binary evidence: {line}")
        elif field == "bytes" and re.fullmatch(r"[1-9][0-9]*", value):
            current[field] = int(value)
        elif field == "sha256" and _HASH.fullmatch(value):
            current[field] = value
        elif field == "identity" and value == "Mach-O 64-bit executable arm64":
            current[field] = value
        else:
            raise ValueError(f"invalid binary evidence: {line}")
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
    if timing_report is None or not is_current_mac_protocol(timing_report):
        raise ValueError("absolute budgets require the paired macOS timing report")
    document = validate_budget_document(budget_path)
    revision = expected_revision
    if timing_report.get("revision") != revision:
        raise ValueError("absolute timing report revision mismatch")
    if timing_report.get("protocol_sha256") != _protocol_sha256(timing_report) or timing_report.get("runner") != RUNNER:
        raise ValueError("absolute timing report protocol mismatch")
    paired_host = timing_report["metadata"]
    _validate_protocol_pair_metadata(paired_host, timing_report["method_version"])
    if paired_host["subject_revision"] != revision or timing_report.get("role") != paired_host["subject_role"]:
        raise ValueError("absolute timing report must use the matching subject arm")
    validate_memory_watchdog_receipt(
        raw_directory.parent / "absolute-capture.txt", paired_host["memory_watchdog_sha256"],
        paired_host["memory_limit_bytes"],
    )
    environment = _validate_environment(
        raw_directory, revision, capture_script_sha256, root_lock_sha256, paired_host, timing_report
    )
    validate_allocation_manifest(allocation_manifest)
    if environment.get("allocation_manifest_sha256") != ALLOCATION_MANIFEST_SHA256:
        raise ValueError("absolute capture allocation manifest hash mismatch")
    sources = paired_host["workload_provenance"]["subject"]["effective"]
    workload_environment_keys = {
        "rca-core/benches/composition.rs": "benchmark_source_sha256",
        "rca-core/src/composition/scope.rs": "scope_zero_test_source_sha256",
        "rca-core/src/composition/combine.rs": "combine_zero_test_source_sha256",
    }
    for source, environment_key in workload_environment_keys.items():
        if environment[environment_key] != sources.get(source):
            raise ValueError(f"absolute capture workload hash mismatch: {source}")
    if environment.get("zero_host_sha256") != paired_host["zero_host_sha256"]:
        raise ValueError("absolute capture zero host hash mismatch")
    timing_rows = {}
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
    allocation_rows = _parse_allocations(
        raw_directory / "allocations.txt", allocation_manifest
    )
    _validate_zero_test(
        raw_directory / "zero-direct-scope.txt",
        "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
    )
    _validate_zero_test(
        raw_directory / "zero-combine.txt", "composition::combine::tests::combine_all_none_widths_do_not_allocate"
    )
    binary_rows = _parse_binaries(raw_directory / "binary-sizes.txt")

    results: list[dict[str, Any]] = []
    for budget_row in document["rows"]:
        scenario = budget_row["scenario"]
        category, name = scenario.split("/", 1)
        statistic = budget_row["measured_statistic"]
        samples: list[float] | None = None
        if category in ("runtime", "compile"):
            samples = [value["value"] for value in timing_rows[(scenario, statistic)]["process_values"]]
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
            "workload_source_sha256": sources[budget_row["workload_source"]],
            "direction": direction,
            "measured_value": value,
            "absolute_budget": budget_row["absolute_budget"],
            "passed": _passes(value, float(budget_row["absolute_budget"]), direction),
        }
        if category == "allocation" and name in {
            "direct_warmed_synchronous", "required_scope_warmed_synchronous",
            "combine_empty_effect_arities_1_through_12",
        }:
            result["product_source_sha256"] = result["workload_source_sha256"]
            result["workload_source"] = "rca-core/tests/composition_qualification/main.rs"
            result["workload_source_sha256"] = paired_host["zero_host_sha256"]
            result["zero_host_sha256"] = paired_host["zero_host_sha256"]
        if samples is not None:
            result["process_values"] = samples
        if category in ("runtime", "compile"):
            result["raw_evidence"] = [value["transcript"] for value in timing_rows[(scenario, statistic)]["process_values"]]
        elif category == "allocation":
            if name in {"direct_warmed_synchronous", "required_scope_warmed_synchronous"}:
                result["raw_evidence"] = ["raw/candidate-absolute/zero-direct-scope.txt"]
            elif name == "combine_empty_effect_arities_1_through_12":
                result["raw_evidence"] = ["raw/candidate-absolute/zero-combine.txt"]
            else:
                result["raw_evidence"] = ["raw/candidate-absolute/allocations.txt"]
        elif category == "binary":
            result["raw_evidence"] = ["raw/candidate-absolute/binary-sizes.txt"]
        results.append(result)

    return {
        "schema_version": 1,
        "suite": "rca-composition-absolute-budgets",
        "timing_method": timing_report["method_version"] + "-subject-arm",
        "candidate_revision": revision,
        "reference_revision": paired_host["control_revision"],
        "reference_kind": paired_host["reference_kind"],
        "runner": RUNNER,
        "toolchain": expected_toolchain(timing_report),
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


def _paired_schedule(protocol: dict[str, Any], block_count: int | None, *, two_look: bool = False) -> list[str]:
    """Keep final evaluation complete while admitting an explicit collection prefix."""
    schedule = timing_schedule(protocol, two_look=two_look)
    if block_count is not None:
        if type(block_count) is not int or not 1 <= block_count <= len(schedule):
            raise ValueError("paired prefix block count is invalid")
        return schedule[:block_count]
    return schedule


def _paired_ratios(repetitions, direction, protocol, block_count=None, *, two_look=False):
    """Validate the same order and value contracts for final and prefix ratios."""
    schedule = _paired_schedule(protocol, block_count, two_look=two_look)
    if len(repetitions) != len(schedule):
        expected = "twenty" if block_count is None else str(len(schedule))
        raise ValueError(f"paired observations require {expected} seeded blocks")
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
    return ratios_by_order


def paired_prefix_bound(repetitions, direction, protocol):
    """Bound the final statistic from below, never accept an incomplete capture.

    Each final stratum has ten observations and uses its second-largest ratio.
    Its observed second-largest ratio cannot decrease as more values arrive.
    This is not a confidence interval for the prefix.
    """
    if (protocol["confidence"]["observations_per_stratum"] != 10
            or protocol["confidence"]["upper_order_statistic_rank"] != 9):
        raise ValueError("paired prefix requires the frozen ten-observation rank-nine rule")
    ratios = _paired_ratios(repetitions, direction, protocol, len(repetitions))
    if any(len(values) < 2 for values in ratios.values()):
        return None
    bounds = {order: sorted(values)[-2] for order, values in ratios.items()}
    lower_bound = math.sqrt(math.prod(bounds.values()))
    return {"final_upper_bound_lower_bound": lower_bound,
            "order_stratum_lower_bounds": bounds,
            "observations_per_order_stratum": {order: len(values) for order, values in ratios.items()},
            "regression_limit": protocol["regression_limit"],
            "irrecoverable": lower_bound > protocol["regression_limit"]}


def paired_prefix_rejections(budget_path, protocol, subject, control, block_count):
    """Apply the unchanged row conversions to a complete, sealed mode prefix."""
    document = validate_budget_document(budget_path)
    failures = []
    runtime = {}
    for row in document["rows"]:
        if row["scenario"].startswith("runtime/"):
            scenario = row["scenario"].removeprefix("runtime/")
            if scenario not in runtime:
                runtime[scenario] = load_isolated_runtime_pairs(subject, control, scenario, protocol,
                                                               block_count=block_count)
            pairs = [{**pair, **{role: _runtime_value(row, {scenario: pair[role]})
                                for role in ("subject", "control")}} for pair in runtime[scenario]]
        elif row["scenario"].startswith("compile/"):
            pairs = load_compile_pairs(subject, control, row["scenario"].split("/")[1], protocol,
                                       block_count=block_count)
        else:
            continue
        bound = paired_prefix_bound(pairs, _direction(row), protocol)
        if bound is not None and bound["irrecoverable"]:
            failures.append({"scenario": row["scenario"], "measured_statistic": row["measured_statistic"], **bound})
    return failures


def paired_ratio_statistics(
    repetitions: list[dict[str, Any]],
    direction: str,
    protocol: dict[str, Any],
) -> dict[str, Any]:
    """Compute the point estimate and exact distribution-free upper bound."""
    ratios_by_order = _paired_ratios(repetitions, direction, protocol)

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


def _command_validate_budget(arguments: argparse.Namespace) -> int:
    document = validate_budget_document(arguments.budget)
    print(json.dumps({"budget_sha256": BUDGET_SHA256, "rows": len(document["rows"])}))
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
    with arguments.output.open("x", encoding="utf-8") as stream:
        json.dump(counters, stream, indent=2, sort_keys=True)
        stream.write("\n")
    return 0


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    validate_budget = commands.add_parser("validate-budget")
    validate_budget.add_argument("--budget", type=pathlib.Path, required=True)
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
