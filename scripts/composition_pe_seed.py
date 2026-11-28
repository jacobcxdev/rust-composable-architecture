"""Reuse only current-capture registry dependencies when preparing the six Windows binaries."""

from dataclasses import dataclass
import json
import math
import os
from pathlib import Path
import time

from capture_windows_support import clean_compilation_units
import composition_performance as evidence
import composition_registry_seed as registry
import composition_windows_absolute as absolute


@dataclass(frozen=True)
class WorkspaceSeed:
    units: object
    staging: Path
    scratch: Path
    identities: tuple


def prepare(support, source, cargo_home, staging, scratch, *, started_at, started_unix_ns,
            cargo, lock_sha256, transcript_sha256):
    """Seal the completed on-clock workspace build, never a previous run or a timed cold target."""
    for path in (support, source, staging, scratch):
        if not registry._direct(path).is_dir():
            raise ValueError("expected a current capture directory")
    if (support.parent != staging.parent or support == staging or support.is_relative_to(scratch)
            or scratch.is_relative_to(support) or staging.is_relative_to(scratch)
            or scratch.is_relative_to(staging)):
        raise ValueError("expected sibling evidence roots and a separate scratch directory")
    log = registry._regular(support / "clean-compile.txt")
    donor = registry._direct(support / "compile-target")
    if registry._sha(log) != transcript_sha256:
        raise ValueError("workspace build transcript changed")
    events = [r for line in registry._regular(support / "processes.jsonl").read_text(encoding="utf-8").splitlines()
              if (r := json.loads(line)).get("destination") == str(log)]
    if len(events) != 2:
        raise ValueError("missing a unique workspace build")
    start, end = events
    counters = json.loads(registry._regular(log.with_suffix(".resources.json")).read_text(encoding="utf-8"))
    evidence.validate_windows_resource_counters(counters, expected_kind="compile")
    receipt = counters["process_receipt"]
    direct = counters["direct_process"]
    unix_epoch_100ns = 116444736000000000
    if (type(started_unix_ns) is not int or not 0 < started_unix_ns
            <= (direct["CreationTime100ns"] - unix_epoch_100ns) * 100
            <= (direct["ExitTime100ns"] - unix_epoch_100ns) * 100 <= time.time_ns()):
        raise ValueError("workspace process lifetime is outside the current capture")
    if (start.get("phase") != "start" or end.get("phase") != "finish"
            or start.get("command") != [str(cargo), "build", "--locked", "--workspace", "--all-features", "--features",
                                        "critical-section/std", "--release", "--message-format=json"]
            or start.get("cwd") != str(source) or start.get("target") != str(donor)
            or start.get("incremental") != "0" or end.get("returncode") != 0
            or end.get("process_receipt") != receipt or start.get("command_kind") != "compile"
            or start.get("job_memory_limit_bytes") != receipt["job_memory_limit_bytes"]
            or end.get("job_memory_limit_bytes") != receipt["job_memory_limit_bytes"]
            or not math.isfinite(started_at)
            or not 0 <= started_at <= start["monotonic"] < end["monotonic"] <= time.monotonic()):
        raise ValueError("wrong workspace build, target or process lifetime")
    clean_compilation_units(log)
    for row in registry._records(log):
        if row.get("reason") != "compiler-artifact" or row["package_id"].startswith(registry.REGISTRY + "#"):
            continue
        if (not row["package_id"].startswith("path+")
                or not all(registry._regular(Path(p)).is_relative_to(source)
                           for p in (row["manifest_path"], row["target"]["src_path"]))):
            raise ValueError("foreign source in clean workspace build")
    locks = registry._locks(source / "Cargo.lock", lock_sha256)
    units = registry._seal_registry(donor, log, cargo_home, locks)
    return WorkspaceSeed(units, staging, scratch, (staging.stat(), scratch.stat()))


def install(seed, target, raw, hashes):
    """Install before Cargo touches this PE target. Both fixture and workspace locks are pinned."""
    mode = target.parent.name
    if (mode not in ("candidate-reference", "reference-reference-aa")
            or target != seed.scratch / mode / "absolute-windows-binary"
            or raw != seed.staging / mode / "absolute-windows-raw"
            or not registry._direct(raw).is_dir() or any(registry._direct(target).iterdir())
            or not all(os.path.samestat(s, p.stat()) for s, p in zip(seed.identities, (seed.staging, seed.scratch)))):
        raise ValueError("expected this capture's empty PE target")
    source = target.parent / "source-subject"
    locks = {}
    for path in ("Cargo.lock", "benches/compile/Cargo.lock"):
        for key, pin in registry._locks(source / path, hashes[path]).items():
            if locks.setdefault(key, pin)["checksum"] != pin["checksum"]:
                raise ValueError("workspace and fixture locks disagree")
    return registry._copy_registry(seed.units, target, locks) if seed.units is not None else {}


def verify_build(log, target, source, allowed):
    """Account for every warm output: copied registry bytes or an earlier build in this same PE target."""
    produced = {}
    for row in registry._records(log):
        if row.get("reason") != "compiler-artifact":
            continue
        key, fresh = row["package_id"], row.get("fresh")
        if type(fresh) is not bool:
            raise ValueError("missing Cargo freshness")
        if not key.startswith(registry.REGISTRY + "#") and (
                not key.startswith("path+") or not all(registry._regular(Path(p)).is_relative_to(source)
                for p in (row["manifest_path"], row["target"]["src_path"]))):
            raise ValueError("foreign source in PE build")
        outputs = registry._outputs(row)
        if row.get("executable") is not None and Path(row["executable"]) not in outputs:
            raise ValueError("PE executable was not a reported output of its compiler unit")
        for path in outputs:
            if not registry._regular(path).is_relative_to(target):
                raise ValueError("foreign output in PE build")
            if path in produced:
                raise ValueError("duplicate output in PE build")
            identity = (key, registry._sha(path))
            if fresh and allowed.get(path) != identity:
                raise ValueError("warm PE dependency was not copied or freshly built by this subject")
            produced[path] = identity
    allowed.update(produced)


def capture(capture, protocol, env, monitor, tools, *, seed):
    """Run the unchanged absolute collector; inject dependencies only at its three known PE jobs."""
    staging = Path(capture["staging"])
    source = Path(capture["roots"]["subject"])
    raw = staging / "absolute-windows-raw"
    target = Path(capture["targets"]) / "absolute-windows-binary"
    receipt = registry._regular(staging / "sources/subject.json")
    if registry._sha(receipt) != capture["metadata"]["workload_provenance"]["subject"]["receipt_sha256"]:
        raise ValueError("staged subject receipt changed")
    hashes = json.loads(receipt.read_text(encoding="utf-8"))["effective"]
    jobs = [raw / f"binary-{name}-build.jsonl" for name in ("fixture", "tokio", "app-test")]
    pending = iter(jobs)
    allowed = {}
    original = None

    class PeMonitor:
        def run(self, command, log, cwd, options, **kwargs):
            nonlocal original
            if log not in jobs:
                return monitor.run(command, log, cwd, options, **kwargs)
            if (next(pending, None) != log or cwd != source or source != target.parent / "source-subject"
                    or command[0] != tools["cargo"] or options.get("CARGO_TARGET_DIR") != str(target)):
                raise ValueError("unexpected PE build order, source, tool or target")
            if original is None:
                original = target.stat()
                allowed.update(install(seed, target, raw, hashes))
            if not os.path.samestat(original, registry._direct(target).stat()):
                raise ValueError("replaced PE build target")
            monitor.run(command, log, cwd, {**options, "CARGO_BUILD_BUILD_DIR": str(target)}, **kwargs)
            if not os.path.samestat(original, registry._direct(target).stat()):
                raise ValueError("replaced PE build target")
            verify_build(log, target, source, allowed)

    result = absolute.capture_windows_absolute(capture, protocol, env, PeMonitor(), tools)
    if next(pending, None) is not None:
        raise ValueError("one or more PE builds are missing")
    return result
