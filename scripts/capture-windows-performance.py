#!/usr/bin/env python3
"""Capture fixed paired workloads with Windows process counters.

This script consumes eight existing binaries and their source/build receipt.
Native rows use default-Core binaries; deterministic rows use test-runtime
binaries. All 54 scenarios remain required in one paired cohort.
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
CONFIGURATION = "split-native-deterministic-v1"
BINARY_ROLES = {
    f"{role}_{'deterministic_' if configuration == 'deterministic' else ''}{mode}": (role, configuration, mode)
    for role in ("candidate", "reference") for configuration in ("native", "deterministic")
    for mode in ("normal", "allocation")
}
SHUTDOWN_LIVE_LIMITS = {
    "shutdown/abortive/live/1": 1,
    "shutdown/abortive/live/8": 8,
    "shutdown/abortive/burst/8": 8,
    "shutdown/abortive/burst/64": 64,
}


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
    """Read the pinned 54-scenario manifest, including bounded shutdown memory."""
    scenarios = MANIFEST.read_text(encoding="utf-8").splitlines()
    if len(scenarios) != 54 or len(set(scenarios)) != 54 or any(not value.strip() for value in scenarios):
        raise ValueError("expected 54 distinct qualification scenarios")
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
    if scenario in SHUTDOWN_LIVE_LIMITS:
        return SHUTDOWN_LIVE_LIMITS[scenario]
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


def validate_shutdown_lifetime(value, scenario, samples):
    """Require each bounded batch to retire before the next batch starts."""
    limit = SHUTDOWN_LIVE_LIMITS.get(scenario)
    if limit is None:
        if "shutdown_lifetime" in value:
            raise ValueError("unexpected shutdown lifetime evidence")
        return
    batches = samples + min(samples, 20)
    expected = dict(live_limit=limit, maximum_unfinished=limit, batches=batches,
                    spawned=limit * batches, dropped=limit * batches,
                    unfinished=0, maximum_batch_end_unfinished=0)
    lifetime = value.get("shutdown_lifetime")
    if not isinstance(lifetime, dict) or lifetime.keys() != expected.keys() or any(
            type(lifetime[key]) is not int or lifetime[key] != count
            for key, count in expected.items()):
        raise ValueError("invalid bounded shutdown lifetime evidence")


def validate_measurement(value, scenario, samples, mode):
    """Reject altered workloads, missing measurements and non-finite values."""
    if not isinstance(value, dict) or value.get("scenario") != scenario:
        raise ValueError("workload scenario mismatch")
    for key, expected in (("samples", samples), ("operations", samples * operations_per_sample(scenario))):
        if type(value.get(key)) is not int or value[key] != expected:
            raise ValueError(f"workload {key} mismatch")
    validate_shutdown_lifetime(value, scenario, samples)
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


def validate_source_metadata(value):
    """Require pinned source and tool identities from the separate build step."""
    try:
        for role in ("candidate", "reference"):
            for key, length in (("revision", 40), ("tree", 40), ("lock_sha256", 64),
                                ("manifest_sha256", 64), ("original_harness_sha256", 64)):
                if not re.fullmatch(f"[0-9a-f]{{{length}}}", value[role][key]):
                    raise ValueError(f"invalid {role} {key}")
        if not re.fullmatch("[0-9a-f]{64}", value["harness_sha256"]):
            raise ValueError("invalid common harness hash")
        if not re.fullmatch("[0-9a-f]{64}", value["harness_module_sha256"]):
            raise ValueError("invalid common harness module hash")
        if not re.fullmatch("[0-9a-f]{64}", value["scenario_manifest_sha256"]):
            raise ValueError("invalid scenario manifest hash")
        tools = value["toolchain"]
        if not tools["rustc"].startswith("rustc 1.100.0 ") or not tools["cargo"].startswith("cargo 1.100.0 "):
            raise ValueError("Windows capture requires pinned Rust 1.100.0")
        if tools["target"] != "x86_64-pc-windows-msvc" or not re.fullmatch("[0-9a-f]{40}", tools["rustc_commit"]):
            raise ValueError("invalid Windows toolchain identity")
        if value["candidate"]["revision"] == value["reference"]["revision"]:
            raise ValueError("candidate and reference revisions must differ")
    except (KeyError, TypeError) as error:
        raise ValueError("missing or malformed source/build metadata") from error


def validate_metadata(value):
    """Require common identities and all eight runtime build configurations."""
    validate_source_metadata(value)
    try:
        for role in ("candidate", "reference"):
            if not re.fullmatch("[0-9a-f]{64}", value[role]["tokio_manifest_sha256"]):
                raise ValueError(f"invalid {role} tokio_manifest_sha256")
        validate_configurations(value)
    except (KeyError, TypeError) as error:
        raise ValueError("missing or malformed source/build metadata") from error


def validate_configurations(value):
    """Bind eight build roles to their effective source and actual Core features."""
    if value["configuration"] != CONFIGURATION or set(value["binaries"]) != set(BINARY_ROLES):
        raise ValueError("split qualification requires exactly eight binary roles")
    commands = {item["binary_role"]: item for item in value["commands"]}
    if len(value["commands"]) != 8 or set(commands) != set(BINARY_ROLES):
        raise ValueError("exactly eight distinct build commands are required")
    source_fields = ("source_manifest_sha256", "harness_sha256", "manifest_sha256", "lock_sha256")
    for key, (role, configuration, mode) in BINARY_ROLES.items():
        binary, command = value["binaries"][key], commands[key]
        native = configuration == "native"
        deterministic_features = value[role]["deterministic_core_features"]
        if deterministic_features not in (["default", "test-util"], ["default", "diagnostics", "test-util"]):
            raise ValueError("invalid deterministic Core features")
        expected_features = ["default"] if native else deterministic_features
        if binary["configuration"] != configuration or binary["core_features"] != expected_features:
            raise ValueError(f"binary configuration or Core features differ: {key}")
        for digest in (binary["sha256"], *(binary[field] for field in source_fields),
                       value[f"{role}_source_manifest_sha256"], command["transcript_sha256"]):
            if not re.fullmatch("[0-9a-f]{64}", digest):
                raise ValueError(f"invalid source/build hash: {key}")
        before = {"rca-tokio/benches/qualification.rs": ("harness_sha256", value["harness_sha256"]),
                  "rca-tokio/Cargo.toml": ("manifest_sha256", value[role]["tokio_manifest_sha256"]),
                  "Cargo.lock": ("lock_sha256", value[role]["lock_sha256"])}
        if native:
            expected = {path: {"before_sha256": digest, "after_sha256": binary[field]}
                        for path, (field, digest) in before.items()}
            if binary["transformation"]["files"] != expected:
                raise ValueError(f"native transformation must bind exactly the three staged files: {key}")
        elif (any(binary[field] != digest for field, digest in before.values())
              or binary["source_manifest_sha256"] != value[f"{role}_source_manifest_sha256"]
              or "transformation" in binary):
            raise ValueError(f"deterministic source must retain the original common harness and dependency graph: {key}")
        normal_key = key.removesuffix(mode) + "normal"
        if any(binary[field] != value["binaries"][normal_key][field] for field in (*source_fields, "source_root", "source_manifest")):
            raise ValueError(f"normal/allocation source mismatch: {key}")
        source_root = binary["source_root"].replace("\\", "/").rstrip("/")
        if type(command["returncode"]) is not int or command["returncode"] != 0 or command["binary"]["sha256"] != binary["sha256"]:
            raise ValueError(f"unsuccessful or mismatched build command: {key}")
        for field, name, kind, crate_type, features, test, package, src in (
                ("core_artifact", "rca_core", "lib", "lib", expected_features, False, "rca-core", "src/lib.rs"),
                ("cargo_artifact", "qualification", "bench", "bin", ["qualification-dhat"] if mode == "allocation" else [],
                 True, "rca-tokio", "benches/qualification.rs")):
            artifact = command[field]
            target, profile = artifact["target"], artifact["profile"]
            if (artifact["reason"] != "compiler-artifact" or target["name"] != name or target["kind"] != [kind]
                    or target["crate_types"] != [crate_type] or artifact["features"] != features
                    or artifact["manifest_path"].replace("\\", "/") != f"{source_root}/{package}/Cargo.toml"
                    or target["src_path"].replace("\\", "/") != f"{source_root}/{package}/{src}"
                    or profile["opt_level"] != "3" or profile["debuginfo"] != 2
                    or profile["test"] is not test or profile["debug_assertions"] is not False
                    or profile["overflow_checks"] is not False):
                raise ValueError(f"unexpected compiler artifact features, source or profile: {key}/{field}")
            if test and (artifact["executable"] != binary["path"] or binary["path"] not in artifact["filenames"]):
                raise ValueError(f"compiler artifact does not identify the selected binary: {key}")
            if not test and artifact["executable"] is not None:
                raise ValueError(f"Core artifact is not a library: {key}")
    if value["binaries"]["candidate_normal"]["harness_sha256"] != value["binaries"]["reference_normal"]["harness_sha256"]:
        raise ValueError("candidate/reference native harnesses differ")
    if len({value["binaries"][key]["source_root"] for key in BINARY_ROLES if key.endswith("_normal")}) != 4:
        raise ValueError("four distinct candidate/reference native/deterministic source roots are required")


def validate_build_transcripts(source):
    """Verify the selected Core and benchmark artifacts in each pinned build log."""
    pins = {}
    for command in source["commands"]:
        path = Path(command["transcript"]).resolve(strict=True)
        if sha256(path) != command["transcript_sha256"]:
            raise ValueError("build transcript hash differs from source/build receipt")
        selected = {"rca_core": [], "qualification": []}
        finished, last = [], None
        with path.open(encoding="utf-8") as stream:
            for line in stream:
                if not line.lstrip().startswith("{"):
                    continue
                record = json.loads(line)
                last = record
                if record.get("reason") == "build-finished":
                    finished.append(record)
                if record.get("reason") == "compiler-artifact":
                    target = record.get("target", {})
                    name = target.get("name")
                    if (name == "rca_core" and target.get("kind") == ["lib"]
                            or name == "qualification" and target.get("kind") == ["bench"]):
                        selected[name].append(record)
        if selected != {"rca_core": [command["core_artifact"]], "qualification": [command["cargo_artifact"]]}:
            raise ValueError("build transcript does not contain the unique selected compiler artifacts")
        if len(finished) != 1 or finished[0].get("success") is not True or last != finished[0]:
            raise ValueError("build transcript lacks one successful terminal build-finished record")
        pins[path] = command["transcript_sha256"]
    return pins


def native_staged_files(original):
    """Derive the reviewed native-only harness and remove its two test-runtime edges."""
    def once(text, old, new):
        if text.count(old) != 1:
            raise ValueError("native staging input does not match the fixed transformation")
        return text.replace(old, new, 1)

    harness = original["rca-tokio/benches/qualification.rs"].decode("utf-8")
    for declaration in ("use rca_test::{DEFAULT_STEP_BUDGET, DeterministicRuntime};",
                        "fn measure_sync<F>(", "fn deterministic(", "struct InfiniteSelfWake;",
                        "impl Future for InfiniteSelfWake {"):
        harness = once(harness, declaration, "#[cfg(any())]\n" + declaration)
    for name in ("ready/1", "ready/16", "ready/256", "timer/1", "timer/1000", "budget"):
        harness = once(harness, f'    "deterministic/{name}",\n', "")
    harness = once(harness, '    if let Some(kind) = scenario.strip_prefix("deterministic/") {\n'
                           '        return deterministic(kind, samples);\n    }\n', "")
    manifest = once(original["rca-tokio/Cargo.toml"], b"rca-test.workspace = true\n", b"")
    lock = original["Cargo.lock"]
    packages = list(re.finditer(rb'(?ms)^\[\[package\]\]\nname = "rca-tokio"\n.*?(?=^\[\[package\]\]|\Z)', lock))
    if len(packages) != 1:
        raise ValueError("native staging requires one rca-tokio lockfile package")
    package = packages[0]
    lock = lock[:package.start()] + once(package.group(), b' "rca-test",\n', b"") + lock[package.end():]
    return {"rca-tokio/benches/qualification.rs": harness.encode("utf-8"),
            "rca-tokio/Cargo.toml": manifest, "Cargo.lock": lock}


def validate_staged_sources(source):
    """Check existing full inventories and permit only the three native staging edits."""
    roots, inventories = set(), {}
    for key, (role, configuration, mode) in BINARY_ROLES.items():
        if mode != "normal":
            continue
        binary = source["binaries"][key]
        root = Path(binary["source_root"]).resolve(strict=True)
        roots.add(root)
        manifest = Path(binary["source_manifest"]).resolve(strict=True)
        if sha256(manifest) != binary["source_manifest_sha256"]:
            raise ValueError("source inventory hash differs from source/build receipt")
        inventory = json.loads(manifest.read_text(encoding="utf-8"))
        if not isinstance(inventory, dict) or not inventory:
            raise ValueError("source inventory must contain the complete relative-path hash map")
        for relative, digest in inventory.items():
            path = root / relative
            if (Path(relative).is_absolute() or ".." in Path(relative).parts or path.is_symlink()
                    or not path.resolve(strict=True).is_relative_to(root)
                    or not isinstance(digest, str) or not re.fullmatch("[0-9a-f]{64}", digest)
                    or sha256(path) != digest):
                raise ValueError(f"staged source differs from its inventory: {relative}")
        if {path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file() or path.is_symlink()} != set(inventory):
            raise ValueError("staged source membership differs from its inventory")
        for relative, digest in (("Cargo.toml", source[role]["manifest_sha256"]),
                                 ("Cargo.lock", binary["lock_sha256"]),
                                 ("rca-tokio/Cargo.toml", binary["manifest_sha256"]),
                                 ("rca-tokio/benches/qualification.rs", binary["harness_sha256"]),
                                 ("rca-tokio/benches/qualification/shutdown.rs", source["harness_module_sha256"])):
            if inventory.get(relative) != digest:
                raise ValueError(f"source inventory does not bind {relative}")
        inventories[role, configuration] = inventory
    if len(roots) != 4:
        raise ValueError("staged source roots must resolve to four distinct directories")
    for role in ("candidate", "reference"):
        native, deterministic = inventories[role, "native"], inventories[role, "deterministic"]
        changed = {path for path in native.keys() | deterministic.keys() if native.get(path) != deterministic.get(path)}
        expected = set(source["binaries"][f"{role}_normal"]["transformation"]["files"])
        if native.keys() != deterministic.keys() or changed != expected:
            raise ValueError("native staging changed source outside the exact three-file transformation")
        root = Path(source["binaries"][f"{role}_deterministic_normal"]["source_root"])
        transformed = native_staged_files({path: (root / path).read_bytes() for path in expected})
        if any(native[path] != hashlib.sha256(content).hexdigest() for path, content in transformed.items()):
            raise ValueError("native source differs from the fixed three-file transformation")


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
    if set(binaries) != set(BINARY_ROLES):
        raise ValueError("exactly eight candidate/reference native/deterministic normal/allocation binaries are required")
    binaries = {key: path.resolve(strict=True) for key, path in binaries.items()}
    if len(set(binaries.values())) != 8 or any(not path.is_file() for path in binaries.values()):
        raise ValueError("eight distinct binary files are required")
    identities = {key: {"path": str(path), "sha256": sha256(path)} for key, path in binaries.items()}
    if any(identity["sha256"] != source["binaries"][key]["sha256"] for key, identity in identities.items()):
        raise ValueError("binary role hash differs from source/build receipt")
    if source["harness_sha256"] != sha256(ROOT / "rca-tokio/benches/qualification.rs"):
        raise ValueError("common harness differs from source/build receipt")
    if source["scenario_manifest_sha256"] != sha256(MANIFEST):
        raise ValueError("scenario manifest differs from source/build receipt")
    build_pins = validate_build_transcripts(source)
    validate_staged_sources(source)
    provider = WindowsCounters()
    host = host_metadata()
    scenarios = load_scenarios()
    recorded_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    script_hash, manifest_hash = sha256(Path(__file__)), sha256(MANIFEST)
    source_hash = hashlib.sha256(source_bytes).hexdigest()
    protocol_hash = hashlib.sha256((source["harness_sha256"] + "\n" + source["harness_module_sha256"] + "\n"
                                   + manifest_hash + "\n" + script_hash + "\n" + source_hash + "\n").encode()).hexdigest()
    runner = f"windows-native-{host.get('hostname', 'unknown')}-{source['toolchain']['target']}"
    cohort = {"id": hashlib.sha256(uuid.uuid4().bytes).hexdigest(), "recorded_at": recorded_at, "runner": runner,
              "compiler": source["toolchain"]["rustc"], "cargo": source["toolchain"]["cargo"],
              "rustc_commit": source["toolchain"]["rustc_commit"], **host,
              "protocol_sha256": protocol_hash, "qualification_source_sha256": source["harness_sha256"],
              "qualification_module_sha256": source["harness_module_sha256"],
              "qualification_script_sha256": script_hash, "scenario_manifest_sha256": manifest_hash,
              "configuration": CONFIGURATION, "source_build_sha256": source_hash}
    for role in ("candidate", "reference"):
        for name, key in (("source_tree", "tree"), ("lock_sha256", "lock_sha256"),
                          ("manifest_sha256", "manifest_sha256"), ("original_harness_sha256", "original_harness_sha256")):
            cohort[f"{role}_{name}"] = source[role][key]
    metadata = {**host, "compiler": cohort["compiler"], "cargo": cohort["cargo"],
                "rustc_commit": cohort["rustc_commit"], "resource_provider": PROVIDER,
                "cohort": cohort, "method": {"aggregation": "geometric mean of order-stratified paired medians",
                "normal_repetitions": 10, "allocation_repetitions": 6,
                "pair": {f"{role}_revision": source[role]["revision"] for role in ("candidate", "reference")}},
                "binaries": identities, "source_build": source, "configuration": CONFIGURATION,
                "source_build_sha256": source_hash}
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
            expected = scenarios if BINARY_ROLES[key][1] == "deterministic" else [name for name in scenarios if not name.startswith("deterministic/")]
            if stem.with_suffix(".stdout").read_text(encoding="utf-8").splitlines() != expected:
                raise ValueError(f"scenario manifest mismatch: {key}")
        for index, scenario in enumerate(scenarios):
            configuration = "deterministic" if scenario.startswith("deterministic/") else "native"
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
                        binary_key = f"{role}_{'deterministic_' if configuration == 'deterministic' else ''}{mode}"
                        counters = run_process([str(binaries[binary_key]), scenario, str(samples)], stem,
                                               process_env, timeout, provider)
                        value = json.loads(stem.with_suffix(".stdout").read_text(encoding="utf-8"))
                        validate_measurement(value, scenario, samples, mode)
                        validate_counters(counters)
                        if mode == "allocation":
                            profile = json.loads(stem.with_suffix(".dhat.json").read_text(encoding="utf-8"))
                            if profile.get("dhatFileVersion") != 2 or not isinstance(profile.get("pps"), list):
                                raise ValueError("missing or malformed DHAT profile")
                        value.update(repetition=repetition, order=order, configuration=configuration)
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
        if any(sha256(path) != digest for path, digest in build_pins.items()):
            raise ValueError("build transcript changed during capture")
        validate_staged_sources(source)
        documents = {}
        for role in rows:
            document = {"schema_version": 6, "suite": "rca-performance-qualification",
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
    for key in BINARY_ROLES:
        parser.add_argument(f"--{key.replace('_', '-')}", required=True, type=Path)
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--timeout-seconds", type=float, default=1800)
    args = parser.parse_args()
    binaries = {key: getattr(args, key) for key in BINARY_ROLES}
    capture(binaries, args.metadata, args.output.resolve(), args.timeout_seconds)


if __name__ == "__main__":
    main()
