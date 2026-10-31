#!/usr/bin/env python3
"""Capture fixed paired workloads with Windows process counters.

This script consumes four existing binaries and their source/build receipt.
It does not build code, compare products, retry failures or publish evidence.
PeakWorkingSetSize measures peak resident working-set bytes. PeakPagefileUsage
measures peak process commit charge, not physical memory or macOS footprint.
QueryProcessCycleTime includes user and kernel cycles across process threads.
Instructions are unsupported and remain null. Counter definitions:
https://learn.microsoft.com/windows/win32/api/psapi/ns-psapi-process_memory_counters
https://learn.microsoft.com/windows/win32/api/realtimeapiset/nf-realtimeapiset-queryprocesscycletime
"""

import argparse
import copy
import ctypes
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import statistics
import subprocess
import sys
import uuid


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "docs/performance/scenarios.txt"
PROVIDER = "win32-process-v1"


class FileTime(ctypes.Structure):
    """Keep the Windows FILETIME layout independent of host C-long width."""

    _fields_ = [("dwLowDateTime", ctypes.c_uint32), ("dwHighDateTime", ctypes.c_uint32)]

    def ticks(self):
        """Return the original count of 100-nanosecond units."""
        return self.dwLowDateTime | (self.dwHighDateTime << 32)


class ProcessMemoryCounters(ctypes.Structure):
    """Match the documented PROCESS_MEMORY_COUNTERS structure."""

    _fields_ = [("cb", ctypes.c_uint32), ("PageFaultCount", ctypes.c_uint32)] + [
        (name, ctypes.c_size_t) for name in (
            "PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
            "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage",
            "PagefileUsage", "PeakPagefileUsage")]


class WindowsCounters:
    """Read process-lifetime counters before the retained handle closes."""

    def __init__(self, kernel32=None, psapi=None):
        if kernel32 is None:
            if sys.platform != "win32":
                raise OSError("Windows capture requires Windows")
            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            psapi = ctypes.WinDLL("psapi", use_last_error=True)
        self.kernel32, self.psapi = kernel32, psapi
        kernel32.QueryProcessCycleTime.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint64)]
        kernel32.QueryProcessCycleTime.restype = ctypes.c_int32
        kernel32.GetProcessTimes.argtypes = [ctypes.c_void_p] + [ctypes.POINTER(FileTime)] * 4
        kernel32.GetProcessTimes.restype = ctypes.c_int32
        psapi.GetProcessMemoryInfo.argtypes = [ctypes.c_void_p, ctypes.POINTER(ProcessMemoryCounters), ctypes.c_uint32]
        psapi.GetProcessMemoryInfo.restype = ctypes.c_int32

    def read(self, handle):
        """Fail if any required API fails or returns invalid counters."""
        cycles = ctypes.c_uint64()
        creation, exit_time, kernel, user = (FileTime() for _ in range(4))
        memory = ProcessMemoryCounters()
        memory.cb = ctypes.sizeof(memory)
        calls = (
            ("QueryProcessCycleTime", self.kernel32.QueryProcessCycleTime,
             (handle, ctypes.byref(cycles))),
            ("GetProcessTimes", self.kernel32.GetProcessTimes,
             (handle, *(ctypes.byref(value) for value in (creation, exit_time, kernel, user)))),
            ("GetProcessMemoryInfo", self.psapi.GetProcessMemoryInfo,
             (handle, ctypes.byref(memory), memory.cb)),
        )
        for name, function, arguments in calls:
            if not function(*arguments):
                raise OSError(ctypes.get_last_error(), f"{name} failed")
        result = {
            "QueryProcessCycleTime": cycles.value,
            "CreationTime100ns": creation.ticks(), "ExitTime100ns": exit_time.ticks(),
            "KernelTime100ns": kernel.ticks(), "UserTime100ns": user.ticks(),
            "PeakWorkingSetSize": memory.PeakWorkingSetSize,
            "PeakPagefileUsage": memory.PeakPagefileUsage,
            "PageFaultCount": memory.PageFaultCount, "instructions_retired": None,
        }
        validate_counters(result)
        return result


def write_json(path, value):
    """Create evidence without replacing an earlier file."""
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write("\n")


def sha256(path):
    """Hash a complete input without loading a binary into memory."""
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run_process(command, stem, env, timeout, provider):
    """Retain raw output and counters, terminating only this child on timeout."""
    with stem.with_suffix(".stdout").open("xb") as stdout, stem.with_suffix(".stderr").open("xb") as stderr:
        process = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=env)
        try:
            try:
                process.wait(timeout=timeout)
            except BaseException:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
                raise
            if process.returncode != 0:
                raise subprocess.CalledProcessError(process.returncode, command)
            # CPython retains this Windows handle after wait(), unlike reopening
            # by PID after exit. It belongs only to the child created above.
            counters = provider.read(int(process._handle))
            write_json(stem.with_suffix(".counters.json"), counters)
            return counters
        finally:
            process._handle.Close()


def load_scenarios():
    """Read the existing pinned 50-scenario manifest."""
    scenarios = MANIFEST.read_text(encoding="utf-8").splitlines()
    if len(scenarios) != 50 or len(set(scenarios)) != 50 or any(not value.strip() for value in scenarios):
        raise ValueError("expected 50 distinct qualification scenarios")
    return scenarios


def allocation_samples(scenario):
    """Use the same ordered sample rules as the native qualification script."""
    if scenario.endswith("/1000"):
        return 200
    if scenario == "deterministic/budget":
        return 5
    if scenario.startswith(("mailbox/", "shutdown/")) or scenario.endswith("/64"):
        return 500
    return 1000


def operations_per_sample(scenario):
    """Match the units asserted by the common qualification workload."""
    if scenario.startswith("external_commit/"):
        parts = scenario.split("/")
        return int(parts[2].split("_")[0]) * (8 if parts[3] == "saturated" else 1)
    if scenario.startswith("mailbox/"):
        return 34
    if scenario == "action_task/finish":
        return 3
    if scenario == "deterministic/budget":
        return 100000
    if scenario.startswith(("independent_stores/", "observation/fanout/", "cancellation/",
                            "action_task/fanout/", "timer/insert/", "deterministic/")):
        return int(scenario.rsplit("/", 1)[1])
    return 1


def validate_measurement(value, scenario, samples, mode):
    """Reject altered workloads, missing measurements and non-finite values."""
    if not isinstance(value, dict) or value.get("scenario") != scenario:
        raise ValueError("workload scenario mismatch")
    for key, expected in (("samples", samples), ("operations", samples * operations_per_sample(scenario))):
        if type(value.get(key)) is not int or value[key] != expected:
            raise ValueError(f"workload {key} mismatch")
    for key in ("allocations", "allocated_bytes", "task_wakes", "peak_bytes"):
        if type(value.get(key)) is not int or value[key] < 0:
            raise ValueError(f"invalid workload metric: {key}")
    throughput = value.get("throughput_per_second")
    if type(throughput) not in (float, int) or not math.isfinite(throughput) or throughput <= 0:
        raise ValueError("invalid throughput")
    latency = value.get("latency_ns", {})
    values = [latency.get(key) for key in ("p50", "p95", "p99")] if isinstance(latency, dict) else []
    if len(values) != 3 or any(type(item) is not int or item <= 0 for item in values) or values != sorted(values):
        raise ValueError("invalid latency percentiles")
    if mode == "normal" and any(value[key] for key in ("allocations", "allocated_bytes", "peak_bytes")):
        raise ValueError("normal measurement contains allocator instrumentation")
    if mode == "allocation" and any(value[key] == 0 for key in ("allocations", "allocated_bytes", "peak_bytes")):
        raise ValueError("allocation measurement contains no heap evidence")
    if value["task_wakes"] == 0:
        raise ValueError("workload recorded no task polls")


def validate_counters(value):
    """Require native units without manufacturing unsupported instructions."""
    positive = ("QueryProcessCycleTime", "CreationTime100ns", "ExitTime100ns", "PeakWorkingSetSize", "PeakPagefileUsage")
    for key in (*positive, "KernelTime100ns", "UserTime100ns", "PageFaultCount"):
        if type(value.get(key)) is not int or value[key] < (1 if key in positive else 0):
            raise ValueError(f"invalid Windows process counter: {key}")
    if value["ExitTime100ns"] < value["CreationTime100ns"] or value.get("instructions_retired", False) is not None:
        raise ValueError("invalid process lifetime or unsupported instruction counter")


def validate_metadata(value):
    """Require pinned source and tool identities from the separate build step."""
    try:
        for role in ("candidate", "reference"):
            for key, length in (("revision", 40), ("tree", 40), ("lock_sha256", 64),
                                ("manifest_sha256", 64), ("original_harness_sha256", 64)):
                if not re.fullmatch(f"[0-9a-f]{{{length}}}", value[role][key]):
                    raise ValueError(f"invalid {role} {key}")
        if not re.fullmatch("[0-9a-f]{64}", value["harness_sha256"]):
            raise ValueError("invalid common harness hash")
        if not re.fullmatch("[0-9a-f]{64}", value["scenario_manifest_sha256"]):
            raise ValueError("invalid scenario manifest hash")
        for role in ("candidate", "reference"):
            for mode in ("normal", "allocation"):
                if not re.fullmatch("[0-9a-f]{64}", value["binaries"][f"{role}_{mode}"]["sha256"]):
                    raise ValueError("invalid build binary hash")
        tools = value["toolchain"]
        if not tools["rustc"].startswith("rustc 1.98.0 ") or not tools["cargo"].startswith("cargo 1.98.0 "):
            raise ValueError("Windows capture requires pinned Rust 1.98.0")
        if tools["target"] != "x86_64-pc-windows-msvc" or not re.fullmatch("[0-9a-f]{40}", tools["rustc_commit"]):
            raise ValueError("invalid Windows toolchain identity")
        if value["candidate"]["revision"] == value["reference"]["revision"]:
            raise ValueError("candidate and reference revisions must differ")
    except (KeyError, TypeError) as error:
        raise ValueError("missing or malformed source/build metadata") from error


def host_metadata():
    """Record Windows and CPU identity without changing host scheduling."""
    if sys.platform != "win32":
        raise OSError("Windows capture requires Windows")
    import winreg
    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0") as key:
        cpu = winreg.QueryValueEx(key, "ProcessorNameString")[0].strip()
    return {"system": "Windows", "os": platform.platform(), "machine_model": platform.machine(),
            "cpu": cpu, "logical_cpus": os.cpu_count(), "hostname": platform.node(),
            "python": sys.version, "python_executable": sys.executable}


def aggregate(normal, allocation, pairs, revision, runner):
    """Keep reporting medians separate from the complete paired observations."""
    row = copy.deepcopy(normal[0])
    for key in ("repetition", "order", "windows_process_counters"):
        row.pop(key, None)
    for key in ("throughput_per_second", "task_wakes", "cpu_cycles", "peak_working_set_bytes", "peak_commit_bytes"):
        row[key] = statistics.median(item[key] for item in normal)
    row["latency_ns"] = {key: statistics.median(item["latency_ns"][key] for item in normal) for key in ("p50", "p95", "p99")}
    for key in ("allocations", "allocated_bytes", "peak_bytes"):
        row[key] = statistics.median(item[key] for item in allocation)
    row.update(revision=revision, runner=runner, allocation_samples=allocation[0]["samples"],
               allocation_operations=allocation[0]["operations"], normal_repetitions=10,
               allocation_repetitions=6, pairs=pairs, rationale_id=None)
    return row


def capture(binaries, metadata_path, output, timeout):
    """Run one fixed cohort, retaining incomplete evidence on every failure."""
    if output.exists():
        raise FileExistsError(output)
    if not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("process timeout must be positive")
    source_bytes = metadata_path.read_bytes()
    source = json.loads(source_bytes.decode("utf-8"))
    validate_metadata(source)
    expected_keys = {f"{role}_{mode}" for role in ("candidate", "reference") for mode in ("normal", "allocation")}
    if set(binaries) != expected_keys:
        raise ValueError("exactly four candidate/reference normal/allocation binaries are required")
    binaries = {key: path.resolve(strict=True) for key, path in binaries.items()}
    if len(set(binaries.values())) != 4 or any(not path.is_file() for path in binaries.values()):
        raise ValueError("four distinct binary files are required")
    identities = {key: {"path": str(path), "sha256": sha256(path)} for key, path in binaries.items()}
    if any(identity["sha256"] != source["binaries"][key]["sha256"] for key, identity in identities.items()):
        raise ValueError("binary role hash differs from source/build receipt")
    if source["harness_sha256"] != sha256(ROOT / "rca-tokio/benches/qualification.rs"):
        raise ValueError("common harness differs from source/build receipt")
    if source["scenario_manifest_sha256"] != sha256(MANIFEST):
        raise ValueError("scenario manifest differs from source/build receipt")
    provider = WindowsCounters()
    host = host_metadata()
    scenarios = load_scenarios()
    recorded_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    script_hash, manifest_hash = sha256(Path(__file__)), sha256(MANIFEST)
    protocol_hash = hashlib.sha256((source["harness_sha256"] + "\n" + manifest_hash + "\n" + script_hash + "\n").encode()).hexdigest()
    runner = f"windows-native-{host.get('hostname', 'unknown')}-{source['toolchain']['target']}"
    cohort = {"id": hashlib.sha256(uuid.uuid4().bytes).hexdigest(), "recorded_at": recorded_at, "runner": runner,
              "compiler": source["toolchain"]["rustc"], "cargo": source["toolchain"]["cargo"],
              "rustc_commit": source["toolchain"]["rustc_commit"], **host,
              "protocol_sha256": protocol_hash, "qualification_source_sha256": source["harness_sha256"],
              "qualification_script_sha256": script_hash, "scenario_manifest_sha256": manifest_hash}
    for role in ("candidate", "reference"):
        for name, key in (("source_tree", "tree"), ("lock_sha256", "lock_sha256"),
                          ("manifest_sha256", "manifest_sha256"), ("original_harness_sha256", "original_harness_sha256")):
            cohort[f"{role}_{name}"] = source[role][key]
    metadata = {**host, "compiler": cohort["compiler"], "cargo": cohort["cargo"],
                "rustc_commit": cohort["rustc_commit"], "resource_provider": PROVIDER,
                "cohort": cohort, "method": {"aggregation": "geometric mean of order-stratified paired medians",
                "normal_repetitions": 10, "allocation_repetitions": 6,
                "pair": {f"{role}_revision": source[role]["revision"] for role in ("candidate", "reference")}},
                "binaries": identities, "source_build": source,
                "source_build_sha256": hashlib.sha256(source_bytes).hexdigest()}
    output.mkdir()
    raw = output / "raw"
    raw.mkdir()
    with (output / "source-build.json").open("xb") as stream:
        stream.write(source_bytes)
    write_json(output / "inputs.json", {"metadata": metadata, "scenarios": scenarios, "timeout_seconds": timeout})
    rows = {"candidate": [], "reference": []}
    env = {key: value for key, value in os.environ.items() if key not in ("RCA_QUALIFY_ALLOCATIONS", "RCA_DHAT_FILE")}
    try:
        for key, binary in binaries.items():
            stem = raw / f"{key}-list"
            run_process([str(binary), "--list"], stem, env, timeout, provider)
            if stem.with_suffix(".stdout").read_text(encoding="utf-8").splitlines() != scenarios:
                raise ValueError(f"scenario manifest mismatch: {key}")
        for index, scenario in enumerate(scenarios):
            observations = {role: {"normal": [], "allocation": []} for role in rows}
            pairs = {"normal": [], "allocation": []}
            for mode, repetitions, samples in (("normal", 10, 10000), ("allocation", 6, allocation_samples(scenario))):
                for repetition in range(1, repetitions + 1):
                    order = "reference-candidate" if repetition % 2 else "candidate-reference"
                    pair = {"repetition": repetition, "order": order}
                    for role in order.split("-"):
                        stem = raw / f"{role}-{mode}-{index:02d}-{repetition:02d}"
                        process_env = dict(env)
                        if mode == "allocation":
                            process_env.update(RCA_QUALIFY_ALLOCATIONS="1", RCA_DHAT_FILE=str(stem.with_suffix(".dhat.json")))
                        counters = run_process([str(binaries[f"{role}_{mode}"]), scenario, str(samples)], stem,
                                               process_env, timeout, provider)
                        value = json.loads(stem.with_suffix(".stdout").read_text(encoding="utf-8"))
                        validate_measurement(value, scenario, samples, mode)
                        validate_counters(counters)
                        if mode == "allocation":
                            profile = json.loads(stem.with_suffix(".dhat.json").read_text(encoding="utf-8"))
                            if profile.get("dhatFileVersion") != 2 or not isinstance(profile.get("pps"), list):
                                raise ValueError("missing or malformed DHAT profile")
                        value.update(repetition=repetition, order=order)
                        if mode == "normal":
                            value.update(cpu_cycles=counters["QueryProcessCycleTime"], instructions_retired=None,
                                         peak_working_set_bytes=counters["PeakWorkingSetSize"],
                                         peak_commit_bytes=counters["PeakPagefileUsage"], windows_process_counters=counters)
                        observations[role][mode].append(value)
                        pair[role] = value
                    pairs[mode].append(pair)
                    write_json(raw / f"pair-{mode}-{index:02d}-{repetition:02d}.json", pair)
            for role in rows:
                rows[role].append(aggregate(observations[role]["normal"], observations[role]["allocation"], pairs,
                                            source[role]["revision"], runner))
            write_json(raw / f"completed-row-{index:02d}.json", {role: rows[role][-1] for role in rows})
        if any(sha256(path) != identities[key]["sha256"] for key, path in binaries.items()):
            raise ValueError("binary changed during capture")
        if sha256(metadata_path) != metadata["source_build_sha256"] or sha256(Path(__file__)) != script_hash or sha256(MANIFEST) != manifest_hash:
            raise ValueError("capture inputs changed")
        if sha256(ROOT / "rca-tokio/benches/qualification.rs") != source["harness_sha256"]:
            raise ValueError("common harness changed during capture")
        documents = {}
        for role in rows:
            document = {"schema_version": 5, "suite": "rca-performance-qualification",
                        "reference_kind": "qualification" if role == "candidate" else "paired-reference",
                        "revision": source[role]["revision"], "runner": runner, "recorded_at": recorded_at,
                        "accepted_regressions": [], "metadata": metadata, "supporting_metrics": {}, "rows": rows[role]}
            write_json(output / f"{role}.json", document)
            documents[role] = document
        write_json(output / "complete.json", {"complete": True, "rows": len(scenarios), "cohort_id": cohort["id"]})
        return documents["candidate"]
    except BaseException as error:
        write_json(output / "failure.json", {"complete": False, "error": type(error).__name__, "message": str(error),
                                            "completed_rows": len(rows["candidate"])})
        raise


def main():
    """Parse one fixed Windows capture request."""
    parser = argparse.ArgumentParser(description=__doc__)
    for role in ("candidate", "reference"):
        for mode in ("normal", "allocation"):
            parser.add_argument(f"--{role}-{mode}", required=True, type=Path)
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--timeout-seconds", type=float, default=1800)
    args = parser.parse_args()
    binaries = {f"{role}_{mode}": getattr(args, f"{role}_{mode}")
                for role in ("candidate", "reference") for mode in ("normal", "allocation")}
    capture(binaries, args.metadata, args.output.resolve(), args.timeout_seconds)


if __name__ == "__main__":
    main()
