#!/usr/bin/env python3
"""Run the frozen composition protocol on its one authorised Darwin host."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
from datetime import datetime, timezone
import json
import io
import math
import os
from pathlib import Path
import signal
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib

sys.dont_write_bytecode = True
import composition_performance as evidence  # noqa: E402 -- suppress bytecode before local imports
import common_outcome_allocations as common_outcome_evidence  # noqa: E402


ROOT = Path(__file__).resolve().parents[1]
PROTOCOL = ROOT / "docs/performance/composition/composition-protocol-v7.json"
BUDGET = ROOT / "docs/performance/composition/composition-budgets.json"
EVIDENCE_ROOT = ROOT / "target/qualification-evidence/0.1.0/composition"
LOCK = Path("/tmp/rca-composition-performance.machine.lock")
AA_RECEIPT_FILES = (
    "protocol.json",
    "plan.json",
    "metadata.json",
    "inputs.json",
    "comparison.json",
    "absolute.json",
    "complete.json",
)

# Only consumer workloads and their test-only support can replace archived inputs.
COMMON_HARNESS_FILES = {
    path: path for path in (
        "rca-core/benches/composition.rs",
        "benches/compile/direct/src/main.rs",
        "benches/compile/composed-manual/src/main.rs",
        "benches/compile/composed-macro/src/main.rs",
        "benches/compile/scoped/src/main.rs",
        "benches/compile/support/src/lib.rs",
        "examples/composition-search/src/main.rs",
        "examples/composition-search/src/lib.rs",
    )
} | {
    "examples/composition-search/tests/app.rs": "scripts/fixtures/composition-app.rs",
    "rca-core/tests/composition_qualification/main.rs": "scripts/fixtures/composition-zero.rs",
    "rca-core/tests/composition_qualification/composition-zero-runner.rs": "rca-core/src/allocation_test/runner.rs",
}


def source_hashes(root):
    """Identify every staged input; Cargo outputs live outside this source directory."""
    return {str(path.relative_to(root)): evidence._sha256(path)
            for path in sorted(root.rglob("*"))
            if path.is_file()}


def apply_common_harness(controller, product, files):
    """Copy the reviewed workload allowlist without replacing product implementation."""
    if set(files) != set(COMMON_HARNESS_FILES) or any(
        row.get("source") != COMMON_HARNESS_FILES.get(name) for name, row in files.items()
    ):
        raise ValueError("common workload allowlist differs from the reviewed protocol")
    for row in files.values():
        source = controller / row["source"]
        if source.is_symlink() or evidence._sha256(source) != row["sha256"]:
            raise ValueError(f"common workload source hash mismatch: {source}")
    original = source_hashes(product)
    for name, row in files.items():
        destination = product / name
        if destination.is_symlink() or not destination.resolve().is_relative_to(product.resolve()):
            raise ValueError("common workload destination escapes the staged source")
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(controller / row["source"], destination)
    return {"original": original, "effective": source_hashes(product)}


def stage_source(source, identity, destination, protocol):
    """Archive one exact revision and add only the frozen consumer harness and adapters."""
    destination.mkdir()
    archive = subprocess.check_output(["git", "archive", "--format=tar", identity["revision"]], cwd=source)
    with tarfile.open(fileobj=io.BytesIO(archive)) as stream:
        stream.extractall(destination, filter="data")
    receipt = apply_common_harness(ROOT, destination, protocol["common_workloads"]["files"])
    manifest = destination / "rca-core/Cargo.toml"
    with manifest.open("a", encoding="utf-8") as stream:
        stream.write('\n[[test]]\nname = "composition_zero"\npath = "tests/composition_qualification/main.rs"\n'
                     'harness = false\nrequired-features = ["allocation-measurement"]\n')
    adapters = ["external-zero-host-v1"]
    if identity["revision"] != evidence.REFERENCE_REVISION:
        fixture = destination / "examples/composition-search/tests/app.rs"
        text = fixture.read_text()
        old = "TestStore::builder(initial_state(), reducer)"
        if text.count(old) != 1:
            raise ValueError("common example constructor adapter input differs")
        text = text.replace(old, "TestStore::projected_builder(initial_state(), reducer, |_| ())")
        old_bound = "R: rca::Reducer<State = rca_example_composition_search::AppState, Action = AppAction>,"
        if text.count(old_bound) != 1:
            raise ValueError("common example reducer adapter input differs")
        fixture.write_text(text.replace(old_bound, old_bound[:-1] + " + Send + 'static,"))
        adapters.append("unit-projection-constructor-v1")
    receipt.update(root=str(destination), revision=identity["revision"],
                   tree=identity["tree"], adapters=adapters,
                   common_workloads=protocol["common_workloads"]["files"],
                   effective=source_hashes(destination))
    return receipt


def validate_staged_source(receipt_path):
    """Reject changed staged inputs before capture and before accepting its evidence."""
    receipt = json.loads(Path(receipt_path).read_text())
    root = Path(receipt["root"])
    changed = {name for name in receipt["original"].keys() | receipt["effective"].keys()
               if receipt["original"].get(name) != receipt["effective"].get(name)}
    if changed - (COMMON_HARNESS_FILES.keys() | {"rca-core/Cargo.toml"}):
        raise ValueError("staged source replaced product implementation")
    if source_hashes(root) != receipt["effective"]:
        raise ValueError("staged composition source changed")
    return receipt


def read(command, cwd=None, env=None, timeout=30):
    return subprocess.check_output(command, cwd=cwd, env=env, text=True, timeout=timeout).strip()


def write_json(path, value):
    with path.open("x", encoding="utf-8") as stream:
        json.dump(value, stream, indent=2, sort_keys=True, allow_nan=False)
        stream.write("\n")


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def report_paths(value, staging):
    """Keep report evidence links relative to the published directory."""
    if isinstance(value, dict):
        return {key: report_paths(item, staging) for key, item in value.items()}
    if isinstance(value, list):
        return [report_paths(item, staging) for item in value]
    if isinstance(value, str):
        if value.startswith(str(staging) + "/"):
            return str(Path(value).relative_to(staging))
        if value.startswith("raw/candidate-absolute/"):
            return value.replace("raw/candidate-absolute/", "absolute-raw/", 1)
    return value


@contextmanager
def machine_lock(path):
    try:
        path.mkdir(mode=0o700)
    except FileExistsError as error:
        raise ValueError(
            f"whole-run lock is held: {path}; inspect its owner, never steal it"
        ) from error
    try:
        write_json(path / "owner.json", {"pid": os.getpid(), "recorded_at": now()})
        yield
    finally:
        (path / "owner.json").unlink(missing_ok=True)
        path.rmdir()


def publish(staged, target):
    """Darwin's atomic no-replace rename, including empty-directory collisions."""
    if sys.platform != "darwin":
        raise OSError("atomic publication requires Darwin renamex_np")
    libc = ctypes.CDLL(None, use_errno=True)
    rename = libc.renamex_np
    rename.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint]
    rename.restype = ctypes.c_int
    if rename(os.fsencode(staged), os.fsencode(target), 0x00000004) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(target))


def output_paths(value):
    output = Path(value).absolute()
    if output.is_symlink():
        raise ValueError("output must not be a symlink")
    output = output.resolve()
    if (
        not output.is_relative_to(EVIDENCE_ROOT.resolve())
        or output == EVIDENCE_ROOT.resolve()
    ):
        raise ValueError(f"output must be below {EVIDENCE_ROOT}")
    staging = output.with_name(output.name + ".incomplete")
    if output.exists() or staging.exists() or staging.is_symlink():
        raise ValueError("output or incomplete staging already exists")
    return output, staging


def validate_aa_evidence(value, protocol, controller, host, *, capture=None):
    path = Path(value).absolute().resolve()
    if protocol["method_version"] == "composition-paired-v7" and capture is None:
        raise ValueError("A/A evidence needs its contemporaneous capture")
    if capture is not None:
        if path != (capture / "reference-reference-aa").resolve():
            raise ValueError("A/A evidence is outside its contemporaneous capture")
        validate_interleaved_capture(capture, protocol)
    if (
        capture is None and (not path.is_relative_to(EVIDENCE_ROOT.resolve())
        or path == EVIDENCE_ROOT.resolve()
        or path.name.endswith(".incomplete"))
    ):
        raise ValueError("A/A evidence must be a published qualification directory")
    try:
        receipts = {
            name: json.loads((path / name).read_text(encoding="utf-8"))
            for name in AA_RECEIPT_FILES
            if name != "protocol.json"
        }
    except (OSError, json.JSONDecodeError) as error:
        raise ValueError(f"cannot read A/A evidence: {path}") from error
    if evidence._sha256(path / "protocol.json") != evidence._protocol_sha256(protocol):
        raise ValueError("A/A evidence protocol hash differs from current protocol")
    if receipts["plan.json"] != evidence.measurement_plan(
        protocol, "reference-reference-aa"
    ):
        raise ValueError("A/A evidence measurement plan differs from current protocol")
    metadata = evidence._load_protocol_metadata(
        path / "metadata.json", protocol, path / "plan.json"
    )
    evidence.validate_protocol_sources(path, metadata, protocol)
    inputs = receipts["inputs.json"]
    comparison = receipts["comparison.json"]
    absolute = receipts["absolute.json"]
    complete = receipts["complete.json"]
    if not all(
        isinstance(item, dict) for item in (inputs, comparison, absolute, complete)
    ):
        raise ValueError("A/A evidence receipt is malformed")
    if not all(
        isinstance(inputs.get(key), dict)
        for key in ("controller", "host", "subject", "control")
    ):
        raise ValueError("A/A evidence input identity is malformed")
    comparison_rows = comparison.get("rows")
    absolute_rows = absolute.get("rows")
    if not all(
        isinstance(rows, list) and all(isinstance(row, dict) for row in rows)
        for rows in (comparison_rows, absolute_rows)
    ):
        raise ValueError("A/A evidence gate rows are malformed")
    expected_relative = sum(
        row["scenario"].startswith(("runtime/", "compile/"))
        for row in evidence.validate_budget_document(BUDGET)["rows"]
    )
    if (
        metadata["mode"] != "reference-reference-aa"
        or metadata["qualification_script_sha256"] != evidence._sha256(Path(__file__))
        or metadata["evaluator_script_sha256"]
        != evidence._sha256(ROOT / "scripts/composition_performance.py")
        or inputs.get("mode") != "reference-reference-aa"
        or inputs.get("controller", {}).get("revision") != controller["revision"]
        or inputs.get("host", {}).get("identity") != host["identity"]
        or any(metadata[key] != host["identity"].get(key) for key in ("os_version", "os_build"))
        or inputs.get("subject", {}).get("revision") != evidence.REFERENCE_REVISION
        or inputs.get("control", {}).get("revision") != evidence.REFERENCE_REVISION
        or complete.get("passed") is not True
        or comparison.get("mode") != "reference-reference-aa"
        or comparison.get("passed") is not True
        or comparison.get("failed_gates") != []
        or comparison.get("metadata") != metadata
        or len(comparison_rows) != expected_relative
        or not all(row.get("passed") is True for row in comparison_rows)
        or absolute.get("mode") != "reference-reference-aa"
        or absolute.get("subject_revision") != evidence.REFERENCE_REVISION
        or absolute.get("reference_only") is not True
        or absolute.get("passed") is not True
        or absolute.get("failed_budgets") != []
        or len(absolute_rows) != 33
        or not all(row.get("passed") is True for row in absolute_rows)
    ):
        raise ValueError("A/A evidence does not prove passing same-host reference gates")
    subject_report, _, checked_comparison = evidence.evaluate_protocol_paired(
        BUDGET,
        protocol,
        path / "paired/subject",
        path / "paired/control",
        path / "plan.json",
        metadata,
    )
    if (
        not checked_comparison["passed"]
        or report_paths(checked_comparison, path) != comparison
    ):
        raise ValueError("A/A evidence relative gates disagree with raw measurements")
    evidence.validate_protocol_diagnostics(
        path / "diagnostics", ("reference-b", "reference-a"), protocol
    )
    checked_absolute = evidence.evaluate_absolute(
        BUDGET,
        path / "absolute-raw",
        evidence.REFERENCE_REVISION,
        metadata["capture_script_sha256"],
        metadata["subject_lock_sha256"],
        timing_report=subject_report,
    )
    checked_absolute["subject_revision"] = checked_absolute.pop("candidate_revision")
    checked_absolute["mode"] = "reference-reference-aa"
    checked_absolute["reference_only"] = True
    if (
        not checked_absolute["passed"]
        or report_paths(checked_absolute, path) != absolute
    ):
        raise ValueError("A/A evidence absolute gates disagree with raw measurements")
    return {
        "path": str(path),
        "sha256": {name: evidence._sha256(path / name) for name in AA_RECEIPT_FILES},
    }


def check_capture_tool(root, cargo, env):
    binary = root / "target/task4-tools/bin/cargo-bloat"
    if not os.access(binary, os.X_OK):
        raise ValueError(f"pinned cargo-bloat 0.12.1 required at {binary}")
    controlled = {
        **env,
        "PATH": f"{binary.parent}:{Path(cargo).parent}:/usr/bin:/bin:/usr/sbin:/sbin",
    }
    if read([cargo, "bloat", "--version"], env=controlled) != "0.12.1":
        raise ValueError(f"pinned cargo-bloat 0.12.1 required at {binary}")
    return binary


def tool_environment(include_capture_tool=True):
    if any(
        name.startswith(("DIVAN_", "CARGO_PROFILE_", "RCA_COMPOSITION_ALLOCATION_"))
        for name in os.environ
    ):
        raise ValueError(
            "composition qualification rejects ambient benchmark overrides"
        )
    overrides = (
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_RUSTC",
        "RUSTC",
        "RUSTDOC",
        "CARGO_INCREMENTAL",
    )
    if any(os.environ.get(name) for name in overrides):
        raise ValueError(
            "composition qualification rejects ambient Rust compiler or build overrides"
        )
    tools = {
        name: read(["rustup", "which", "--toolchain", "1.98.0", name])
        for name in ("cargo", "rustc", "rustdoc")
    }
    versions = {name: read([path, "--version"]) for name, path in tools.items()}
    if (
        versions
        != {
            "cargo": "cargo 1.98.0 (797e8a9bc 2026-08-05)",
            "rustc": "rustc 1.98.0 (88d9e12ae 2026-08-18)",
            "rustdoc": "rustdoc 1.98.0 (88d9e12ae 2026-08-18)",
        }
        or len({str(Path(path).parent) for path in tools.values()}) != 1
    ):
        raise ValueError("exact Cargo, rustc, and rustdoc 1.98.0 required")
    env = dict(os.environ)
    env.update({name.upper(): path for name, path in tools.items()})
    env.update(
        PATH=f"{ROOT}/target/task4-tools/bin:{Path(tools['cargo']).parent}:/usr/bin:/bin:/usr/sbin:/sbin:{os.environ.get('PATH', '')}",
        RUSTC_WRAPPER="",
        RUSTC_WORKSPACE_WRAPPER="",
        CARGO_BUILD_JOBS="1",
    )
    if include_capture_tool:
        check_capture_tool(ROOT, tools["cargo"], env)
    return env, tools


def source_identity(root, reference=False):
    revision = read(["git", "rev-parse", "HEAD"], cwd=root)
    if reference and revision != evidence.REFERENCE_REVISION:
        raise ValueError("checkout is not the frozen reference")
    if read(["git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=root):
        raise ValueError(f"qualification requires a clean checkout: {root}")
    files = {
        "Cargo.lock",
        "benches/compile/Cargo.lock",
        "scripts/capture-composition-performance.sh",
    }
    files.update(
        row["workload_source"]
        for row in evidence.validate_budget_document(BUDGET)["rows"]
    )
    hashes = {}
    for name in sorted(files):
        read(["git", "ls-files", "--error-unmatch", name], cwd=root)
        hashes[name] = evidence._sha256(root / name)
    return {"root": str(root), "revision": revision,
            "tree": read(["git", "rev-parse", "HEAD^{tree}"], cwd=root), "hashes": hashes}


def canonical_host():
    values = {
        "architecture": read(["uname", "-m"]),
        "machine_model": read(["sysctl", "-n", "hw.model"]),
        "cpu_model": read(["sysctl", "-n", "machdep.cpu.brand_string"]),
        "logical_cpus": read(["sysctl", "-n", "hw.logicalcpu"]),
        "memory_bytes": read(["sysctl", "-n", "hw.memsize"]),
        "os_version": read(["sw_vers", "-productVersion"]),
        "os_build": read(["sw_vers", "-buildVersion"]),
        "power_source": read(["pmset", "-g", "batt"]),
        "power_settings": read(["pmset", "-g", "custom"]),
    }
    expected = {
        "architecture": "arm64",
        "machine_model": "Mac15,9",
        "cpu_model": "Apple M3 Max",
        "logical_cpus": "16",
        "memory_bytes": "51539607552",
    }
    ac_settings = values["power_settings"].split("AC Power:")[-1]
    import re

    evidence.validate_measurement_os(values["os_version"])
    if (
        any(values[key] != value for key, value in expected.items())
        or "AC Power" not in values["power_source"]
        or not re.search(r"^\s*powermode\s+2\s*$", ac_settings, re.MULTILINE)
    ):
        raise ValueError(f"host does not match canonical runner {evidence.RUNNER}")
    if os.environ.get("RCA_PERF_RUNNER", evidence.RUNNER) != evidence.RUNNER:
        raise ValueError("RCA_PERF_RUNNER is not canonical")
    return {
        "identity": {**values, "power_source": "AC Power"},
        "telemetry": {"battery": values["power_source"]},
    }


def check_host_unchanged(before, monitor):
    after = canonical_host()
    monitor.event("telemetry.jsonl", {"phase": "final-host", "host": after})
    if after["identity"] != before["identity"]:
        raise ValueError("host identity changed during qualification")


def owned_process_group_exited(pgid):
    """Fail closed unless the owned group is absent or contains only our zombies."""
    try:
        snapshot = subprocess.run(
            ["/bin/ps", "-axo", "pgid=,uid=,stat="],
            capture_output=True, text=True, check=True, timeout=5,
        )
        for line in snapshot.stdout.splitlines():
            group, owner, state = line.split()
            group, owner = int(group), int(owner)
            if group == pgid and (owner != os.getuid() or state[0] != "Z"
                                  or not set(state[1:]) <= set("+<LNs")):
                return False
    except (OSError, subprocess.SubprocessError, ValueError):
        return False
    return True


def stop_owned_process(process):
    """Stop the owned session, including children that outlive its leader."""
    def signal_group(sig):
        try:
            try:
                os.killpg(process.pid, sig)
            except PermissionError:
                # Darwin can report EPERM for an unreaped, exited leader.
                # Reap it, but still signal any surviving descendants.
                if process.poll() is None:
                    raise
                try:
                    os.killpg(process.pid, sig)
                except PermissionError:
                    # Darwin also returns EPERM for a group of unreaped zombies.
                    if not owned_process_group_exited(process.pid):
                        raise
        except ProcessLookupError:
            pass

    try:
        signal_group(signal.SIGTERM)
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    finally:
        signal_group(signal.SIGKILL)
        process.wait(timeout=5)


class HostMonitor:
    def __init__(self, directory, protocol):
        self.directory = directory
        self.protocol = protocol
        self.process_deadline = None
        self.directory.mkdir(parents=True, exist_ok=True)

    def event(self, name, event):
        with (self.directory / name).open("a", encoding="utf-8") as stream:
            stream.write(
                json.dumps(
                    {"recorded_at": now(), "monotonic": time.monotonic(), **event},
                    allow_nan=False,
                )
                + "\n"
            )

    def sample(self, phase, active=False):
        def probe(command):
            remaining = 30 if self.process_deadline is None else self.process_deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(command, 0)
            return read(command, timeout=min(30, remaining))

        data = {"phase": phase, "active": active}
        try:
            data["process_snapshot"] = probe(["ps", "-axo", "pid=,ppid=,comm="])
            data["thermal_history"] = probe(["pmset", "-g", "therm"])
            data["thermal_state"] = probe(
                [
                    "/usr/bin/osascript",
                    "-l",
                    "JavaScript",
                    "-e",
                    'ObjC.import("Foundation"); Number($.NSProcessInfo.processInfo.thermalState)',
                ]
            )
            if data["thermal_state"] != "0":
                raise ValueError("current thermal state is not nominal")
            # Active samples retain load and processes. CPU samples surround each command.
            data["load"] = probe(["sysctl", "-n", "vm.loadavg"])
            data["top"] = (
                "" if active else probe(["top", "-l", "2", "-s", "1", "-n", "0"])
            )
            data["cpu_used_percent"] = None if active else evidence.parse_cpu_used_percent(data["top"])
            data["load_average_1m"] = evidence.parse_load_average_1m(data["load"])
            admitted = evidence.validate_quiescence_sample(
                self.protocol,
                cpu_used_percent=0 if active else data["cpu_used_percent"],
                load_average_1m=0
                if active and self.protocol["quiescence"].get("admission") != "recorded-background-load"
                else data["load_average_1m"],
                thermal_text=data["thermal_history"],
                process_snapshot=data["process_snapshot"],
                own_pid=os.getpid(),
            )
            data["competing_processes"] = admitted["competing_processes"]
            if "AC Power" not in probe(["pmset", "-g", "batt"]):
                raise ValueError("AC power lost during qualification")
            data["passed"] = True
        except Exception as error:
            data.update(passed=False, error=str(error))
            raise
        finally:
            self.event("telemetry.jsonl", data)
        return data

    def idle(self, phase, sustained=False):
        policy = self.protocol["quiescence"]
        if policy.get("admission") == "recorded-background-load":
            self.sample(phase)
            return
        required = policy["consecutive_samples"] if sustained else 1
        count = 0
        deadline = time.monotonic() + policy["timeout_seconds"]
        while count < required:
            if time.monotonic() >= deadline:
                raise ValueError(
                    "sustained quiescence timed out; no measurement retried"
                )
            try:
                self.sample(phase)
                count += 1
            except ValueError:
                count = 0
                required = policy["consecutive_samples"]
            if count < required:
                time.sleep(policy["sample_interval_seconds"])

    def run(self, command, destination, cwd, env, timed=True, timeout=None, counter_receipt=False):
        destination.parent.mkdir(parents=True, exist_ok=True)
        self.idle(f"before:{destination.name}")
        if counter_receipt and not timed:
            raise ValueError('counter receipts require a timed command')
        counter_path = (destination.with_suffix('.time.txt') if counter_receipt else
                        self.directory / f"counter-{time.monotonic_ns()}.txt")
        if counter_receipt:
            if counter_path.exists():
                raise ValueError('compile counter file already exists')
            write_json(destination.with_suffix('.command.json'), dict(command=list(map(str, command)),
                cwd=str(cwd), target=env.get('CARGO_TARGET_DIR'), counter=str(counter_path)))
        executed = (
            ["/usr/bin/time", "-lp", "-o", str(counter_path), *map(str, command)]
            if timed
            else list(map(str, command))
        )
        self.event(
            "processes.jsonl",
            {
                "phase": "start",
                "command": executed,
                "cwd": str(cwd),
                "destination": str(destination),
                "target": env.get("CARGO_TARGET_DIR"),
                "incremental": env.get("CARGO_INCREMENTAL"),
            },
        )
        with destination.open("x") as transcript:
            process = subprocess.Popen(
                executed,
                cwd=cwd,
                env=env,
                stdout=transcript,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            deadline = None if timeout is None else time.monotonic() + timeout
            self.process_deadline = deadline
            try:
                while True:
                    self.sample(f"during:{destination.name}", active=True)
                    remaining = None if deadline is None else deadline - time.monotonic()
                    if remaining is not None and remaining <= 0:
                        raise subprocess.TimeoutExpired(command, timeout)
                    try:
                        interval = self.protocol["quiescence"]["process_monitor_interval_seconds"]
                        process.wait(timeout=interval if remaining is None else min(remaining, interval))
                        break
                    except subprocess.TimeoutExpired:
                        pass
            except BaseException:
                # Terminate the owned process group, including compiler grandchildren.
                stop_owned_process(process)
                raise
            finally:
                self.process_deadline = None
                if counter_receipt:
                    write_json(destination.with_suffix('.exit.json'), {'returncode': process.returncode})
                self.event(
                    "processes.jsonl",
                    {
                        "phase": "finish",
                        "destination": str(destination),
                        "returncode": process.returncode,
                    },
                )
        self.sample(f"after:{destination.name}")
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, command)
        if timed:
            write_json(
                destination.with_suffix(".resources.json"),
                evidence.parse_darwin_time(counter_path.read_text()),
            )


def execute_pairs(protocol, roots, executables, targets, paired, cargo, env, monitor, *, blocks=None):
    schedule = enumerate(evidence.seeded_order_schedule(protocol), 1) if blocks is None else blocks
    for block, order in schedule:
        monitor.idle(f"block-{block:02d}", sustained=True)
        for scenario in protocol["runtime"]["gated_scenarios"]:
            for role in order.split("-"):
                destination = (
                    paired
                    / role
                    / "runtime"
                    / scenario.replace("/", "__")
                    / f"block-{block:02d}.txt"
                )
                monitor.run(
                    [
                        str(executables[role]),
                        "--bench",
                        "--exact",
                        "composition::" + scenario.replace("/", "::"),
                    ],
                    destination,
                    roots[role],
                    env,
                )
                evidence.validate_isolated_transcript(destination, scenario)
        for fixture in protocol["compile"]["fixtures"]:
            for role in order.split("-"):
                target = targets / f"compile-{fixture}-{block:02d}-{role}"
                if target.exists():
                    raise ValueError("compile target is not clean")
                monitor.run(
                    [
                        cargo,
                        "check",
                        "--manifest-path",
                        str(roots[role] / "benches/compile/Cargo.toml"),
                        "-p",
                        f"rca-compile-{fixture}",
                        "--locked",
                        "-j",
                        "1",
                    ],
                    paired / role / "compile" / fixture / f"block-{block:02d}.txt",
                    roots[role],
                    {**env, "CARGO_TARGET_DIR": str(target), "CARGO_INCREMENTAL": "0"},
                )
                # The completed transcript owns the measurements, not this check-only target.
                # Each next sample still receives a distinct, initially absent directory.
                if target.is_symlink():
                    raise ValueError("compile target became a symlink")
                if target.exists():
                    shutil.rmtree(target)


def execute_interleaved_pairs(protocol, captures, cargo, env, monitor, directory):
    """Run each frozen block once, with a control block in the same capture."""
    capture_hash = evidence._sha256(directory / "capture.json")
    with (directory / "blocks.jsonl").open("x", encoding="utf-8") as transcript:
        for block, order in enumerate(evidence.seeded_order_schedule(protocol), 1):
            for mode in evidence.interleaved_mode_order(protocol, block):
                row = {"block": block, "mode": mode, "capture_sha256": capture_hash}
                for phase in ("start", "finish"):
                    measured = {"sha256": paired_block_hashes(directory, protocol, block, mode)} if phase == "finish" else {}
                    transcript.write(json.dumps({**row, **measured, "phase": phase, "recorded_at": now(),
                                                 "monotonic": time.monotonic()}, allow_nan=False) + "\n")
                    transcript.flush()
                    if phase == "start":
                        capture = captures[mode]
                        execute_pairs(protocol, capture["roots"], capture["executables"],
                                      capture["targets"], capture["paired"], cargo, env, monitor,
                                      blocks=[(block, order)])


def paired_block_hashes(directory, protocol, block, mode):
    """Seal each completed transcript and its counters before the next block starts."""
    paths = []
    for role in ("subject", "control"):
        paired = Path(mode) / "paired" / role
        for family, names in (("runtime", protocol["runtime"]["gated_scenarios"]),
                              ("compile", protocol["compile"]["fixtures"])):
            for name in names:
                transcript = paired / family / name.replace("/", "__") / f"block-{block:02d}.txt"
                paths.extend((transcript, transcript.with_suffix(".resources.json")))
    for path in paths:
        if (directory / path).is_symlink():
            raise ValueError("interleaved measurement became a symlink")
    return {str(path): evidence._sha256(directory / path) for path in paths}


def benchmark(root, target, role, staging, tools, env, monitor):
    log = staging / "builds" / f"{role}.txt"
    monitor.run(
        [
            tools["cargo"],
            "bench",
            "--locked",
            "-p",
            "rca-core",
            "--bench",
            "composition",
            "-j",
            "1",
            "--no-run",
            "--message-format=json",
        ],
        log,
        root,
        {**env, "CARGO_TARGET_DIR": str(target)},
    )
    executables = []
    for line in log.read_text().splitlines():
        if line.startswith("{"):
            item = json.loads(line)
            if (
                item.get("reason") == "compiler-artifact"
                and item.get("target", {}).get("name") == "composition"
                and item.get("executable")
            ):
                executables.append(Path(item["executable"]).resolve())
    if (
        len(executables) != 1
        or not executables[0].is_relative_to(target.resolve())
        or not os.access(executables[0], os.X_OK)
    ):
        raise ValueError(
            "build did not produce exactly one isolated composition executable"
        )
    return executables[0]


def prepare_pair_capture(protocol, budget, mode, staging, targets, reference, controller, control,
                         host, tools, env, monitor, hashes):
    """Prepare independently built arms without starting the measured blocks."""
    subject_root = ROOT if mode == "candidate-reference" else reference
    subject = controller if mode == "candidate-reference" else control
    staging.mkdir()
    targets.mkdir()
    write_json(staging / "inputs.json", {
        "mode": mode, "recorded_at": now(), "controller": controller,
        "subject": subject, "control": control, "host": host, "input_hashes": hashes,
        "tools": {name: {"path": path, "sha256": evidence._sha256(Path(path))}
                  for name, path in tools.items()},
    })
    with (staging / "protocol.json").open("xb") as copied_protocol:
        copied_protocol.write(PROTOCOL.read_bytes())
    write_json(staging / "plan.json", evidence.measurement_plan(protocol, mode))
    identities = {"subject": subject, "control": control}
    original_roots = {"subject": subject_root, "control": reference}
    roots = {role: targets / f"source-{role}" for role in original_roots}
    sources = {
        role: stage_source(original_roots[role], identities[role], roots[role], protocol)
        for role in roots
    }
    (staging / "sources").mkdir()
    for role, receipt in sources.items():
        write_json(staging / "sources" / f"{role}.json", receipt)
    executables = {
        role: benchmark(
            roots[role], targets / role, role, staging, tools, env, monitor
        )
        for role in ("control", "subject")
    }
    lists = {}
    for role, executable in executables.items():
        destination = staging / "builds" / f"{role}-list.txt"
        monitor.run([str(executable), "--list"], destination, roots[role], env)
        lists[role] = destination.read_bytes()
    if lists["subject"] != lists["control"]:
        raise ValueError("subject and control benchmark inputs differ")
    roles = (
        {"subject": "candidate", "control": "reference"}
        if mode == "candidate-reference"
        else {"subject": "reference-b", "control": "reference-a"}
    )
    metadata = {
        "schema_version": 1,
        "mode": mode,
        "method_version": protocol["method_version"],
        "protocol_sha256": evidence.PROTOCOL_SHA256,
        "budget_sha256": evidence.BUDGET_SHA256,
        "allocation_manifest_sha256": evidence.ALLOCATION_MANIFEST_SHA256,
        "seed": protocol["randomisation"]["seed"],
        "runner": evidence.RUNNER,
        "toolchain": evidence.TOOLCHAIN,
        "os_version": host["identity"]["os_version"],
        "os_build": host["identity"]["os_build"],
        "plan_sha256": evidence._sha256(staging / "plan.json"),
        "recorded_at": now(),
        "subject_role": roles["subject"],
        "control_role": roles["control"],
        "subject_revision": subject["revision"],
        "control_revision": control["revision"],
        "subject_lock_sha256": subject["hashes"]["Cargo.lock"],
        "control_lock_sha256": control["hashes"]["Cargo.lock"],
        "qualification_script_sha256": evidence._sha256(Path(__file__)),
        "evaluator_script_sha256": evidence._sha256(
            ROOT / "scripts/composition_performance.py"
        ),
        "capture_script_sha256": controller["hashes"][
            "scripts/capture-composition-performance.sh"
        ],
        "workload_sha256": sources["subject"]["effective"]["rca-core/benches/composition.rs"],
        "zero_host_sha256": sources["subject"]["effective"]["rca-core/tests/composition_qualification/main.rs"],
        "workload_provenance": {
            role: {
                **{kind: {name: receipt[kind][name] for name in {
                    row["workload_source"] for row in budget["rows"]}}
                   for kind in ("original", "effective")},
                "adapters": receipt["adapters"],
                "receipt_sha256": evidence._sha256(staging / "sources" / f"{role}.json"),
            } for role, receipt in sources.items()
        },
        "benchmark_list_sha256": evidence._sha256(
            staging / "builds/subject-list.txt"
        ),
        **{
            f"{role}_executable_sha256": evidence._sha256(executable)
            for role, executable in executables.items()
        },
    }
    return {"mode": mode, "staging": staging, "targets": targets, "subject": subject,
            "roots": roots, "sources": sources, "executables": executables,
            "roles": roles, "metadata": metadata, "paired": staging / "paired"}


def finish_pair_capture(capture, protocol, controller, env, monitor):
    """Evaluate all paired and absolute gates, preserving failed measurements."""
    mode, staging, targets = capture["mode"], capture["staging"], capture["targets"]
    subject, roots = capture["subject"], capture["roots"]
    executables, roles, metadata = capture["executables"], capture["roles"], capture["metadata"]
    for role, executable in executables.items():
        monitor.run(
            [str(executable), "--bench"],
            staging / "diagnostics" / f"{roles[role]}.txt",
            roots[role],
            env,
        )
    evidence.validate_protocol_diagnostics(
        staging / "diagnostics", tuple(roles.values()), protocol
    )
    subject_report, control_report, comparison = evidence.evaluate_protocol_paired(
        BUDGET,
        protocol,
        staging / "paired/subject",
        staging / "paired/control",
        staging / "plan.json",
        evidence._load_protocol_metadata(
            staging / "metadata.json", protocol, staging / "plan.json"
        ),
    )
    reports = {
        f"{roles['subject']}.json": subject_report,
        f"{roles['control']}.json": control_report,
        "comparison.json": comparison,
    }
    capture_env = {
        key: value
        for key, value in env.items()
        if not key.startswith("CARGO_PROFILE_")
    }
    capture_env["CARGO_TARGET_DIR"] = str(targets / "absolute")
    raw = staging / "absolute-raw"
    monitor.run(
        [
            str(ROOT / "scripts/capture-composition-performance.sh"),
            str(raw),
            str(staging / "sources/subject.json"),
        ],
        staging / "absolute-capture.txt",
        roots["subject"],
        capture_env,
    )
    absolute = evidence.evaluate_absolute(
        BUDGET,
        raw,
        subject["revision"],
        controller["hashes"]["scripts/capture-composition-performance.sh"],
        subject["hashes"]["Cargo.lock"],
        timing_report=subject_report,
    )
    captured_environment = evidence._parse_key_values(raw / "environment.txt")
    if (
        captured_environment["fixture_lock_sha256"]
        != subject["hashes"]["benches/compile/Cargo.lock"]
    ):
        raise ValueError("absolute fixture lockfile provenance mismatch")
    absolute["subject_revision"] = absolute.pop("candidate_revision")
    absolute["mode"] = mode
    absolute["reference_only"] = mode == "reference-reference-aa"
    for role, executable in executables.items():
        if evidence._sha256(executable) != metadata[f"{role}_executable_sha256"]:
            raise ValueError("benchmark executable changed during capture")
    reports["absolute.json"] = absolute
    for role in roots:
        if evidence._sha256(staging / "sources" / f"{role}.json") != metadata[
            "workload_provenance"][role]["receipt_sha256"]:
            raise ValueError("staged composition source receipt changed")
        validate_staged_source(staging / "sources" / f"{role}.json")
    for name, report in reports.items():
        write_json(staging / name, report_paths(report, staging))
    passed = all(report.get("passed", True) for report in reports.values())
    write_json(staging / "complete.json", {"completed_at": now(), "passed": passed})
    return passed


def validate_interleaved_capture(directory, protocol):
    """Reject detached controls, changed plans, missing blocks and reordered evidence."""
    manifest = json.loads((directory / "capture.json").read_text())
    digest = evidence._sha256(directory / "capture.json")
    modes = ("candidate-reference", "reference-reference-aa")
    schedule = [{"block": block, "mode": mode}
                for block in range(1, len(evidence.seeded_order_schedule(protocol)) + 1)
                for mode in evidence.interleaved_mode_order(protocol, block)]
    if (not isinstance(manifest, dict) or manifest.get("protocol_sha256") != evidence.PROTOCOL_SHA256
            or manifest.get("schedule") != schedule or not isinstance(manifest.get("modes"), dict)
            or set(manifest["modes"]) != set(modes)):
        raise ValueError("interleaved capture plan differs")
    for mode in modes:
        arm = directory / mode
        if manifest["modes"][mode] != {name: evidence._sha256(arm / name)
                                     for name in ("inputs.json", "plan.json")}:
            raise ValueError("interleaved capture inputs changed")
        inputs = json.loads((arm / "inputs.json").read_text())
        if (not isinstance(inputs, dict) or inputs.get("mode") != mode
                or inputs.get("controller") != manifest.get("controller")
                or inputs.get("host") != manifest.get("host")
                or json.loads((arm / "plan.json").read_text()) != evidence.measurement_plan(protocol, mode)):
            raise ValueError("interleaved capture identity differs")
        metadata = json.loads((arm / "metadata.json").read_text())
        if not isinstance(metadata, dict) or metadata.get("capture_sha256") != digest:
            raise ValueError("interleaved capture binding differs")
    rows = [json.loads(line) for line in (directory / "blocks.jsonl").read_text().splitlines()]
    expected = [{**item, "phase": phase, "capture_sha256": digest}
                for item in schedule for phase in ("start", "finish")]
    if len(rows) != len(expected):
        raise ValueError("interleaved capture has incomplete or extra blocks")
    previous = -1.0
    for row, required in zip(rows, expected):
        measured = {"sha256"} if required["phase"] == "finish" else set()
        if (not isinstance(row, dict)
                or any(type(row.get(key)) is not type(value) or row.get(key) != value
                for key, value in required.items())
                or set(row) != set(required) | {"recorded_at", "monotonic"} | measured
                or not evidence._TIMESTAMP.fullmatch(str(row["recorded_at"]))
                or type(row["monotonic"]) not in (int, float)
                or not math.isfinite(row["monotonic"]) or row["monotonic"] < previous):
            raise ValueError("interleaved capture block order or telemetry differs")
        if measured and row["sha256"] != paired_block_hashes(
            directory, protocol, required["block"], required["mode"]
        ):
            raise ValueError("interleaved measurement bytes changed after capture")
        previous = row["monotonic"]
    return digest


def qualify(output, staging, reference, mode, aa_evidence=None):
    if mode != "candidate-reference" or aa_evidence is not None:
        raise ValueError("composition qualification needs one contemporaneous two-mode capture")
    if sys.platform != "darwin":
        raise ValueError("canonical qualification requires arm64 macOS")
    protocol = evidence.load_protocol(PROTOCOL)
    env, tools = tool_environment()
    host = canonical_host()
    reference = Path(read(["git", "rev-parse", "--show-toplevel"], cwd=reference)).resolve()
    controller = source_identity(ROOT)
    control = source_identity(reference, reference=True)
    if controller["revision"] == control["revision"]:
        raise ValueError("candidate and reference revisions must differ")
    capture_tool = check_capture_tool(ROOT, tools["cargo"], env)
    budget = evidence.validate_budget_document(
        BUDGET, expected_reference=evidence.REFERENCE_REVISION, reference_root=reference)
    if control["hashes"]["scripts/capture-composition-performance.sh"] != budget["runner_source_hash"]:
        raise ValueError("reference capture script provenance mismatch")
    evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    pinned_files = [
        PROTOCOL, BUDGET, evidence.DEFAULT_ALLOCATION_MANIFEST, Path(__file__),
        ROOT / "scripts/composition_performance.py", ROOT / "scripts/qualify-composition-performance.sh",
        *(ROOT / item["source"] for item in protocol["common_workloads"]["files"].values()),
        capture_tool, *(Path(path) for path in tools.values()),
    ]
    hashes = {str(path): evidence._sha256(path) for path in pinned_files}
    staging.parent.mkdir(parents=True, exist_ok=True)
    staging.mkdir()
    monitor = HostMonitor(staging / "telemetry", protocol)
    monitor.idle("initial", sustained=True)
    env.update(CARGO_PROFILE_BENCH_DEBUG="false", CARGO_PROFILE_BENCH_LTO="false",
               CARGO_PROFILE_BENCH_STRIP="true")
    modes = ("candidate-reference", "reference-reference-aa")
    # Build trees are disposable. Transcripts and failed evidence remain in staging.
    with tempfile.TemporaryDirectory(prefix="rca-composition-v7.") as temporary:
        targets = Path(temporary)
        captures = {item: prepare_pair_capture(
            protocol, budget, item, staging / item, targets / item, reference,
            controller, control, host, tools, env, monitor, hashes) for item in modes}
        write_json(staging / "capture.json", {
            "protocol_sha256": evidence.PROTOCOL_SHA256, "controller": controller, "host": host,
            "modes": {item: {name: evidence._sha256(staging / item / name)
                             for name in ("inputs.json", "plan.json")} for item in modes},
            "schedule": [{"block": block, "mode": item}
                         for block in range(1, len(evidence.seeded_order_schedule(protocol)) + 1)
                         for item in evidence.interleaved_mode_order(protocol, block)],
        })
        capture_hash = evidence._sha256(staging / "capture.json")
        for capture in captures.values():
            capture["metadata"]["capture_sha256"] = capture_hash
            write_json(capture["staging"] / "metadata.json", capture["metadata"])
            evidence.validate_protocol_sources(capture["staging"], capture["metadata"], protocol)
        execute_interleaved_pairs(protocol, captures, tools["cargo"], env, monitor, staging)
        passed = {}
        for item in modes:
            passed[item] = finish_pair_capture(captures[item], protocol, controller, env, monitor)
            # Final source and executable hashes are complete. No later sample uses this tree.
            shutil.rmtree(captures[item]["targets"])
        if validate_interleaved_capture(staging, protocol) != capture_hash:
            raise ValueError("interleaved capture manifest changed")
    check_host_unchanged(host, monitor)
    if source_identity(ROOT) != controller or source_identity(reference, reference=True) != control:
        raise ValueError("source or host identity changed during qualification")
    if any(evidence._sha256(Path(path)) != value for path, value in hashes.items()):
        raise ValueError("qualification input hash changed")
    if passed["reference-reference-aa"]:
        validate_aa_evidence(staging / "reference-reference-aa", protocol, controller, host,
                             capture=staging)
    write_json(staging / "complete.json", {
        "completed_at": now(), "passed": all(passed.values()),
        "capture_sha256": capture_hash, "blocks_sha256": evidence._sha256(staging / "blocks.jsonl"),
        "modes": {item: {name: evidence._sha256(staging / item / name)
                         for name in AA_RECEIPT_FILES} for item in modes},
    })
    publish(staging, output)
    return 0 if all(passed.values()) else 1


COUNTER_PROTOCOL = ROOT / "docs/performance/composable-counter-protocol-v1.json"
COUNTER_LIFECYCLE_PROTOCOL = ROOT / "docs/performance/composable-counter-lifecycle-protocol-v1.json"
COUNTER_ALLOCATION_PROTOCOL = ROOT / "docs/performance/composable-counter-allocation-protocol-v1.json"
READY_FUTURE_ALLOCATION_PROTOCOL = ROOT / "docs/performance/composable-ready-future-allocation-protocol-v1.json"
COMMON_OUTCOME_ALLOCATION_PROTOCOL = ROOT / "docs/performance/composable-common-outcome-allocation-protocol-v1.json"
COUNTER_FIXTURES = {"composable": "upstream", "rca": "candidate"}
COUNTER_RELEASE_ENV = {
    "CARGO_INCREMENTAL": "0", "CARGO_BUILD_JOBS": "1",
    "CARGO_PROFILE_RELEASE_OPT_LEVEL": "3", "CARGO_PROFILE_RELEASE_DEBUG": "false",
    "CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS": "false",
    "CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS": "false", "CARGO_PROFILE_RELEASE_LTO": "false",
    "CARGO_PROFILE_RELEASE_PANIC": "unwind", "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "16",
    "CARGO_PROFILE_RELEASE_RPATH": "false", "CARGO_PROFILE_RELEASE_STRIP": "none",
    "CARGO_PROFILE_RELEASE_INCREMENTAL": "false", "CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO": "off",
}


def counter_source_identity(root):
    revision = read(["git", "rev-parse", "HEAD"], cwd=root)
    if read(["git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=root):
        raise ValueError("counter qualification requires a clean checkout")
    names = read(["git", "ls-files", "-z"], cwd=root).split("\0")
    hashes = {}
    for name in filter(None, names):
        path = root / name
        if path.is_symlink() or not path.is_file():
            raise ValueError("counter source must consist of tracked regular files")
        hashes[str(path)] = evidence._sha256(path)
    return {"revision": revision, "hashes": hashes}


def verify_counter_inputs(groups):
    for kind, hashes in groups.items():
        for name, digest in hashes.items():
            path = Path(name)
            if path.is_symlink() or not path.is_file() or evidence._sha256(path) != digest:
                raise ValueError(f"counter {kind} input changed: {path}")


def check_counter_configuration(root, cargo_home):
    configurations = [cargo_home / name for name in ("config", "config.toml")]
    for directory in (root.resolve(), *root.resolve().parents):
        configurations.extend(directory / ".cargo" / name for name in ("config", "config.toml"))
    if any(path.exists() or path.is_symlink() for path in configurations):
        raise ValueError("counter qualification rejects ambient Cargo configuration")


def counter_environment(root, cargo_home=None):
    cargo_home = (cargo_home or Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))).resolve()
    check_counter_configuration(root, cargo_home)
    _, tools = tool_environment(include_capture_tool=False)
    # No inherited compiler, linker, feature or loader settings enter the builds.
    env = {key: os.environ[key] for key in ("HOME", "USER", "LOGNAME", "TMPDIR") if key in os.environ}
    env.update(
        PATH=f"{Path(tools['cargo']).parent}:/usr/bin:/bin:/usr/sbin:/sbin",
        CARGO_HOME=str(cargo_home), RUSTC=tools["rustc"], RUSTDOC=tools["rustdoc"],
        RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="",
        DYLD_LIBRARY_PATH=str(Path(tools["rustc"]).parent.parent / "lib"),
        **COUNTER_RELEASE_ENV,
    )
    return env, tools


def _counter_command(command, destination, env, timeout=600, cwd=ROOT):
    destination.parent.mkdir(parents=True, exist_ok=True)
    write_json(destination.with_suffix(".command.json"), {"command": command, "cwd": str(cwd)})
    with destination.open("x") as stdout, destination.with_suffix(".stderr.txt").open("x") as stderr:
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=stdout,
                                   stderr=stderr, start_new_session=True)
        try:
            process.wait(timeout=timeout)
            if process.returncode:
                raise subprocess.CalledProcessError(process.returncode, command)
        except BaseException:
            stop_owned_process(process)
            raise
        finally:
            write_json(destination.with_suffix(".exit.json"), {"returncode": process.returncode})


def counter_dependency_identity(metadata, lock_path, cargo_home):
    """Authenticate compiled dependency files against locked Git objects or crate archives."""
    lock = tomllib.loads(lock_path.read_text())
    locked = {(item["name"], item["version"], item.get("source")): item
              for item in lock["package"]}
    hashes = {}
    for package in metadata["packages"]:
        source = package.get("source")
        directory = Path(package["manifest_path"]).resolve().parent
        if source is None:
            if not directory.is_relative_to(ROOT.resolve()):
                raise ValueError("counter path dependency is outside the frozen source")
            continue
        identity = (package["name"], package["version"], source)
        if identity not in locked or not directory.is_relative_to(cargo_home.resolve()):
            raise ValueError("counter dependency is outside its locked private cache")
        if source.startswith("git+"):
            checkout = Path(read(["git", "rev-parse", "--show-toplevel"], cwd=directory))
            revision = source.rsplit("#", 1)[-1]
            if (not checkout.is_relative_to(cargo_home.resolve() / "git/checkouts")
                    or read(["git", "rev-parse", "HEAD"], cwd=checkout) != revision):
                raise ValueError("counter dependency checkout does not match its locked revision")
            status = read(["git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=checkout)
            if any(line != "?? .cargo-ok" for line in status.splitlines()):
                raise ValueError("counter dependency checkout differs from its locked source")
            names = read(["git", "ls-files", "-z"], cwd=checkout).split("\0")
            paths = [checkout / name for name in names if name]
        elif source.startswith("registry+"):
            if directory.parent.parent != cargo_home.resolve() / "registry/src":
                raise ValueError("counter registry dependency cache path is invalid")
            archive = cargo_home / "registry/cache" / directory.parent.name / (directory.name + ".crate")
            if evidence._sha256(archive) != locked[identity].get("checksum"):
                raise ValueError("counter dependency archive does not match its locked checksum")
            expected = set()
            with tarfile.open(archive, "r:gz") as stream:
                for member in stream:
                    name = Path(member.name)
                    if not name.parts or name.parts[0] != directory.name or ".." in name.parts:
                        raise ValueError("counter dependency archive path is invalid")
                    if member.isdir():
                        continue
                    if not member.isfile():
                        raise ValueError("counter dependency archive contains a non-regular file")
                    path = directory.joinpath(*name.parts[1:])
                    data = stream.extractfile(member)
                    if path.is_symlink() or data is None or path.read_bytes() != data.read():
                        raise ValueError("counter dependency bytes differ from the locked archive")
                    expected.add(path)
            paths = [path for path in directory.rglob("*") if path.is_file() or path.is_symlink()]
            if set(paths) - expected - {directory / ".cargo-ok", directory / ".cargo-checksum.json"}:
                raise ValueError("counter dependency contains extra source files")
            hashes[str(archive)] = evidence._sha256(archive)
        else:
            raise ValueError("counter dependency source type is unsupported")
        for path in paths:
            if path.is_symlink() or not path.is_file():
                raise ValueError("counter dependency source is not a regular file")
            hashes[str(path)] = evidence._sha256(path)
    return hashes


def build_counter_binaries(staging, targets, protocol, env, tools, *, features=(), ready_futures=False,
                           minimal_size=False, common_outcome=False):
    binaries = {}
    working_directory = targets.parent
    cargo_home = Path(env["CARGO_HOME"])
    dependency_hashes = {}
    build_receipts = {}
    feature_args = ["--features", ",".join(features)] if features else []
    family = "composable-stream" if ready_futures or common_outcome else "composable-counter"
    if minimal_size:
        evidence._validate_counter_document(protocol, evidence._MINIMAL_SIZE_DOCUMENT_SHA256)
        if features or ready_futures or common_outcome or any(env.get(k) != v for k, v in protocol["profile"].items()):
            raise ValueError("minimal size build features or profile differ")

    def build_command(command, destination, environment, cwd):
        _counter_command(command, destination, environment, cwd=cwd)
        if minimal_size:
            validate_minimal_command(destination, command, cwd)

    for engine, fixture in COUNTER_FIXTURES.items():
        if minimal_size:
            verify_counter_inputs({"build_receipts": build_receipts})
        check_counter_configuration(working_directory, cargo_home)
        manifest = ROOT / "tests" / family / fixture / "Cargo.toml"
        base = [tools["cargo"], "--manifest-path", str(manifest)]
        build_command([base[0], "fetch", *base[1:], "--locked", "--target", protocol["target"]],
                         staging / "builds" / f"{fixture}-fetch.txt", env, cwd=working_directory)
        build_command([base[0], "metadata", *base[1:], "--locked", "--offline",
                          "--format-version", "1", "--filter-platform", protocol["target"],
                          *feature_args],
                         staging / "builds" / f"{fixture}-metadata.json", env, cwd=working_directory)
        metadata_path = staging / "builds" / f"{fixture}-metadata.json"
        if minimal_size:
            validate_minimal_metadata(json.loads(metadata_path.read_text()), engine, protocol)
        current = counter_dependency_identity(json.loads(metadata_path.read_text()),
                                              manifest.with_name("Cargo.lock"), cargo_home)
        verify_counter_inputs({"dependencies": dependency_hashes})
        dependency_hashes.update(current)
        if minimal_size:
            verify_counter_inputs({"build_receipts": build_receipts})
            build_receipts.update({str(path): evidence._sha256(path)
                                   for path in (staging / "builds").iterdir() if path.is_file()})
        binaries[engine] = {}
        for role in ("a", "b"):
            if minimal_size:
                verify_counter_inputs({"build_receipts": build_receipts})
            target = targets / f"{fixture}-{role}"
            if target.exists():
                raise ValueError("counter build target is not fresh")
            binary_name = ("common-outcome-allocations" if common_outcome else
                           "minimal-counter" if minimal_size else f"{family}-{fixture}")
            command = [base[0], "build", *base[1:], "--locked", "--offline", "--release",
                       "--bin", binary_name,
                       "--target", protocol["target"], "-j", "1", *feature_args]
            build_command(command, staging / "builds" / f"{fixture}-{role}.txt",
                             {**env, "CARGO_TARGET_DIR": str(target)}, cwd=working_directory)
            if minimal_size:
                verify_counter_inputs({"build_receipts": build_receipts})
                for path in (staging / "builds").glob(f"{fixture}-{role}.*"):
                    build_receipts[str(path)] = evidence._sha256(path)
            check_counter_configuration(working_directory, cargo_home)
            verify_counter_inputs({"dependencies": dependency_hashes})
            binary = target / protocol["target"] / "release" / binary_name
            binaries[engine][role] = {"path": str(binary), "sha256": evidence._sha256(binary)}
    write_json(staging / "dependency-hashes.json", dependency_hashes)
    if minimal_size:
        verify_counter_inputs({"build_receipts": build_receipts})
        write_json(staging / "build-receipt-hashes.json", build_receipts)
    return binaries


def _counter_plan(protocol, lifecycle):
    return (evidence.counter_lifecycle_measurement_plan(protocol) if lifecycle
            else evidence.counter_measurement_plan(protocol))


def _counter_raw(value, expected_engine, lifecycle):
    if lifecycle:
        raw = evidence.validate_composable_counter_lifecycle(value)
        engine = raw["engine"]
    else:
        raw = evidence.validate_composable_counter_timing(value)
        engine = raw["work"]["engine"]
    if engine != expected_engine:
        raise ValueError("counter raw evidence came from the wrong engine")
    return raw


def capture_counter_programme(staging, protocol, binaries, env, monitor, *, lifecycle=False):
    records = []
    for cohort in _counter_plan(protocol, lifecycle):
        for block in cohort["blocks"]:
            monitor.idle(f"{cohort['name']}:{block['block']}", sustained=True)
            for arm in block["order"].split("-"):
                engine = cohort[f"{arm}_engine"]
                binary = binaries[engine]["a" if arm == "control" else "b"]
                verify_counter_inputs({"binary": {binary["path"]: binary["sha256"]}})
                destination = staging / "raw" / cohort["name"] / f"{block['block']:02d}-{arm}.json"
                flag = "--measure-lifecycle" if lifecycle else "--measure"
                monitor.run([binary["path"], flag], destination, ROOT, env,
                            timed=False, timeout=protocol["process_timeout_seconds"])
                _counter_raw(json.loads(destination.read_text()), engine, lifecycle)
                verify_counter_inputs({"binary": {binary["path"]: binary["sha256"]}})
                record = {"cohort": cohort["name"], **block, "arm": arm,
                          "path": str(destination.relative_to(staging)),
                          "sha256": evidence._sha256(destination), "exit_code": 0}
                records.append(record)
                write_json(destination.with_suffix(".receipt.json"), {**record, "binary": binary})
    return records


def load_counter_capture(staging, protocol, records, *, lifecycle=False):
    expected = [(cohort, block, arm)
                for cohort in _counter_plan(protocol, lifecycle)
                for block in cohort["blocks"] for arm in block["order"].split("-")]
    if type(records) is not list or len(records) != len(expected):
        raise ValueError("counter capture is incomplete, including required A/A controls")
    cohorts = {cohort["name"]: [] for cohort in _counter_plan(protocol, lifecycle)}
    seen = set()
    for record, (cohort, block, arm) in zip(records, expected):
        identity = {"cohort": cohort["name"], **block, "arm": arm, "exit_code": 0}
        if (type(record) is not dict or record.keys() != identity.keys() | {"path", "sha256"}
                or any(type(record[key]) is not type(value) or record[key] != value
                       for key, value in identity.items())):
            raise ValueError("counter process receipt identity is invalid")
        relative = Path(record["path"])
        path = staging / relative
        if (relative.is_absolute() or ".." in relative.parts or path.is_symlink()
                or not path.resolve().is_relative_to(staging.resolve()) or path.resolve() in seen):
            raise ValueError("counter raw evidence path is unsafe or reused")
        seen.add(path.resolve())
        verify_counter_inputs({"raw": {str(path): record["sha256"]}})
        raw = _counter_raw(json.loads(path.read_text()), cohort[f"{arm}_engine"], lifecycle)
        rows = cohorts[cohort["name"]]
        if len(rows) < block["block"]:
            rows.append(dict(block))
        rows[-1][arm] = raw
    return cohorts


def capture_counter_allocation_programme(staging, protocol, binaries, env, *, ready_futures=False,
                                         common_outcome=False):
    """Capture every allocation process without invoking timing host gates."""
    records = []
    plan = (common_outcome_evidence.measurement_plan(protocol) if common_outcome else
            evidence.ready_future_allocation_measurement_plan(protocol) if ready_futures else
            evidence.counter_allocation_measurement_plan(protocol))
    arguments = [] if common_outcome else ["--measure-ready-future-allocations" if ready_futures else "--measure-allocations"]
    decode = (common_outcome_evidence.decode if common_outcome else
              evidence.decode_ready_future_allocations if ready_futures else evidence.decode_counter_allocations)
    for item in plan:
        binary = binaries[item["engine"]][item["role"]]
        verify_counter_inputs({"binary": {binary["path"]: binary["sha256"]}})
        destination = staging / "raw" / f"{item['sequence']:02d}.json"
        _counter_command([binary["path"], *arguments], destination, env,
                         timeout=protocol["process_timeout_seconds"], cwd=ROOT)
        decode(destination.read_text(), item["engine"])
        verify_counter_inputs({"binary": {binary["path"]: binary["sha256"]}})
        record = dict(item, path=str(destination.relative_to(staging)),
                      sha256=evidence._sha256(destination), exit_code=0,
                      process_sha256={suffix: evidence._sha256(destination.with_suffix(suffix))
                                      for suffix in (".command.json", ".stderr.txt", ".exit.json")})
        records.append(record)
        write_json(destination.with_suffix(".receipt.json"), {**record, "binary": binary})
    return records


def load_counter_allocation_capture(staging, protocol, records, binaries, *, ready_futures=False,
                                   common_outcome=False):
    """Admit every ordered raw object and its successful process evidence."""
    plan = (common_outcome_evidence.measurement_plan(protocol) if common_outcome else
            evidence.ready_future_allocation_measurement_plan(protocol) if ready_futures else
            evidence.counter_allocation_measurement_plan(protocol))
    arguments = [] if common_outcome else ["--measure-ready-future-allocations" if ready_futures else "--measure-allocations"]
    decode = (common_outcome_evidence.decode if common_outcome else
              evidence.decode_ready_future_allocations if ready_futures else evidence.decode_counter_allocations)
    if type(records) is not list or len(records) != len(plan):
        raise ValueError("counter allocation capture is incomplete")
    rows = []
    for record, item in zip(records, plan):
        identity = dict(item, exit_code=0)
        if (type(record) is not dict
                or record.keys() != identity.keys() | {"path", "sha256", "process_sha256"}
                or any(type(record[key]) is not type(value) or record[key] != value
                       for key, value in identity.items())):
            raise ValueError("counter allocation process receipt identity is invalid")
        relative = f"raw/{item['sequence']:02d}.json"
        if type(record["path"]) is not str or record["path"] != relative:
            raise ValueError("counter allocation raw evidence path is invalid")
        path = staging / relative
        if not path.resolve().is_relative_to(staging.resolve()):
            raise ValueError("counter allocation raw evidence path escapes capture")
        process_hashes = record["process_sha256"]
        if type(process_hashes) is not dict or process_hashes.keys() != {
                ".command.json", ".stderr.txt", ".exit.json"}:
            raise ValueError("counter allocation process evidence is incomplete")
        verify_counter_inputs({"raw": {str(path): record["sha256"]},
                               "process": {str(path.with_suffix(suffix)): digest
                                           for suffix, digest in process_hashes.items()}})
        binary = binaries[item["engine"]][item["role"]]
        verify_counter_inputs({"binary": {binary["path"]: binary["sha256"]}})
        expected_command = {"command": [binary["path"], *arguments], "cwd": str(ROOT)}
        if json.loads(path.with_suffix(".command.json").read_text()) != expected_command:
            raise ValueError("counter allocation command receipt is invalid")
        exit_receipt = json.loads(path.with_suffix(".exit.json").read_text())
        if (type(exit_receipt) is not dict or exit_receipt.keys() != {"returncode"}
                or type(exit_receipt["returncode"]) is not int or exit_receipt["returncode"] != 0):
            raise ValueError("counter allocation process did not exit successfully")
        rows.append(dict(item, raw=decode(path.read_text(), item["engine"])))
    return rows


def qualify_counter(output, staging, *, lifecycle=False, allocations=False, ready_futures=False,
                    common_outcome=False):
    staging.parent.mkdir(parents=True, exist_ok=True)
    staging.mkdir()
    try:
        if ready_futures and not allocations:
            raise ValueError("ready future capture requires allocation mode")
        if common_outcome and (not allocations or ready_futures):
            raise ValueError("common outcome requires its own allocation mode")
        if lifecycle and allocations:
            raise ValueError("counter measurement modes are mutually exclusive")
        allocation_options = ({"common_outcome": True} if common_outcome else
                              {"ready_futures": True} if ready_futures else {})
        protocol_path = (COMMON_OUTCOME_ALLOCATION_PROTOCOL if common_outcome else
                         READY_FUTURE_ALLOCATION_PROTOCOL if ready_futures else
                         COUNTER_ALLOCATION_PROTOCOL if allocations else
                         COUNTER_LIFECYCLE_PROTOCOL if lifecycle else COUNTER_PROTOCOL)
        protocol = (common_outcome_evidence.load_protocol(protocol_path) if common_outcome else
                    evidence.load_ready_future_allocation_protocol(protocol_path) if ready_futures else
                    evidence.load_counter_allocation_protocol(protocol_path) if allocations else
                    evidence.load_counter_lifecycle_protocol(protocol_path) if lifecycle else
                    evidence.load_counter_protocol(protocol_path))
        source = counter_source_identity(ROOT)
        host = canonical_host()
        with (staging / "protocol.json").open("xb") as stream:
            stream.write(protocol_path.read_bytes())
        write_json(staging / "plan.json", common_outcome_evidence.measurement_plan(protocol)
                   if common_outcome else evidence.ready_future_allocation_measurement_plan(protocol)
                   if ready_futures else evidence.counter_allocation_measurement_plan(protocol)
                   if allocations else _counter_plan(protocol, lifecycle))
        monitor = None if allocations else HostMonitor(staging / "telemetry", evidence.load_protocol(PROTOCOL))
        with tempfile.TemporaryDirectory(prefix="rca-counter-cohorts.", dir="/tmp") as temporary:
            working_directory = Path(temporary)
            cargo_home = working_directory / "cargo-home"
            cargo_home.mkdir()
            env, tools = counter_environment(working_directory, cargo_home)
            tool_hashes = {path: evidence._sha256(Path(path)) for path in tools.values()}
            inputs = {"source": source, "tools": {
                name: {"path": path, "sha256": tool_hashes[path], "version": read([path, "--version"])}
                for name, path in tools.items()}, "host": host, "release_environment": COUNTER_RELEASE_ENV,
                "features": protocol["features"] if allocations else "default",
                "allocator": protocol["instrument"] if allocations else "standard",
                "target": protocol["target"],
                "cargo_home": str(cargo_home), "build_cwd": str(working_directory)}
            write_json(staging / "inputs.json", inputs)
            binaries = build_counter_binaries(
                staging, working_directory / "targets", protocol, env, tools,
                **({"features": tuple(protocol["features"]), **allocation_options} if allocations else {}))
            write_json(staging / "binaries.json", binaries)
            frozen = {"source": source["hashes"], "tools": tool_hashes,
                      "dependencies": json.loads((staging / "dependency-hashes.json").read_text()),
                      "binaries": {value["path"]: value["sha256"]
                                   for roles in binaries.values() for value in roles.values()},
                      "receipts": {str(path): evidence._sha256(path)
                                   for path in staging.rglob("*") if path.is_file()}}
            write_json(staging / "capture-input-hashes.json", frozen)
            verify_counter_inputs(frozen)
            if allocations:
                records = capture_counter_allocation_programme(staging, protocol, binaries, env,
                                                               **allocation_options)
            else:
                monitor.idle("initial-counter", sustained=True)
                capture_options = {"lifecycle": True} if lifecycle else {}
                records = capture_counter_programme(staging, protocol, binaries, env, monitor,
                                                    **capture_options)
            write_json(staging / "records.json", records)
            if allocations:
                frozen["capture"] = {str(path): evidence._sha256(path)
                                     for path in (staging / "raw").rglob("*") if path.is_file()}
                frozen["capture"][str(staging / "records.json")] = evidence._sha256(staging / "records.json")
                observations = load_counter_allocation_capture(staging, protocol, records, binaries,
                                                               **allocation_options)
                result = (common_outcome_evidence.evaluate(observations, protocol) if common_outcome else
                          evidence.evaluate_ready_future_allocations(observations, protocol) if ready_futures
                          else evidence.evaluate_counter_allocations(observations, protocol))
            else:
                cohorts = load_counter_capture(staging, protocol, records, **capture_options)
                result = (evidence.evaluate_counter_lifecycle_cohorts(cohorts, protocol) if lifecycle
                          else evidence.evaluate_counter_cohorts(cohorts, protocol))
            write_json(staging / "analysis.json", result)
            verify_counter_inputs(frozen)
            check_counter_configuration(working_directory, cargo_home)
            if counter_source_identity(ROOT) != source:
                raise ValueError("counter source revision or locks changed during capture")
            if allocations:
                final_host = canonical_host()
                write_json(staging / "final-host.json", final_host)
                if final_host["identity"] != host["identity"]:
                    raise ValueError("host identity changed during allocation capture")
                complete = {"provenance_qualified": True, "allocation_capture_complete": True,
                            "qualified_comparison": False, "recorded_at": now()}
            else:
                check_host_unchanged(host, monitor)
                # Keep the numerical report unqualified; only this receipt attests capture.
                complete = {"provenance_qualified": True,
                            "suitable_for_direction": result["suitable_for_direction"],
                            "qualified_comparison": result["suitable_for_direction"],
                            "recorded_at": now()}
                if not result["suitable_for_direction"]:
                    write_json(staging / "complete.json", complete)
                    raise ValueError("counter diagnostics unsuitable; estimates retained, no qualified comparison")
        complete["sha256"] = {str(path.relative_to(staging)): evidence._sha256(path)
                              for path in sorted(staging.rglob("*")) if path.is_file()}
        write_json(staging / "complete.json", complete)
        publish(staging, output)
        return 0
    except BaseException as error:
        write_json(staging / "failure.json", {"recorded_at": now(), "error": str(error),
                                             "qualified_comparison": False})
        raise


MINIMAL_SIZE_PROTOCOL = ROOT / "docs/performance/composable-minimal-size-protocol-v1.json"
MINIMAL_COMPILE_PROTOCOL = ROOT / "docs/performance/composable-minimal-compile-protocol-v1.json"


def minimal_compile_command(cargo, manifest, workload, target, architecture):
    if target.exists() or target.is_symlink():
        raise ValueError('minimal compile target is not fresh')
    if workload not in ('check', 'release'):
        raise ValueError('unknown minimal compile workload')
    return [cargo, 'check' if workload == 'check' else 'build', '--manifest-path', str(manifest),
            '--locked', '--offline', '--bin', 'minimal-counter', '--target', architecture, '-j', '1',
            *(['--release'] if workload == 'release' else [])]


def compile_json(path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate minimal compile JSON key')
            result[key] = value
        return result
    return json.loads(path.read_text(), object_pairs_hook=unique)


def minimal_compile_observation(transcript, command, cwd, target, item, original_log=None):
    original_log = original_log or transcript
    if compile_json(transcript.with_suffix('.command.json')) != dict(
            command=command, cwd=str(cwd), target=str(target), counter=str(original_log.with_suffix('.time.txt'))):
        raise ValueError('minimal compile command binding differs')
    status = compile_json(transcript.with_suffix('.exit.json'))
    if status != {'returncode': 0} or type(status['returncode']) is not int:
        raise ValueError('minimal compile command failed')
    text = transcript.read_text()
    if 'stripping debug info' in text and 'failed' in text:
        raise ValueError('minimal compile stripping failed')
    counters = evidence.parse_darwin_time(transcript.with_suffix('.time.txt').read_text())
    parsed = compile_json(transcript.with_suffix('.resources.json'))
    if (parsed != counters or any(type(parsed[key]) is not type(value) for key, value in counters.items())):
        raise ValueError('minimal compile parsed counters differ from raw counters')
    fields = ('real_seconds', 'user_seconds', 'system_seconds', 'maximum_resident_set_bytes')
    return dict(item, counters={key: counters[key] for key in fields})


def capture_minimal_compile(staging, cwd, protocol, env, tools, monitor, frozen):
    """Execute the finite schedule once; failed commands never advance or retry."""
    rows, receipts, binaries = [], {}, {}
    for item in evidence.minimal_compile_plan(protocol):
        block, workload, engine = item['block'], item['workload'], item['engine']
        label = f'{block:02d}-{workload}-{engine}'
        target = cwd / 'targets' / label
        manifest = ROOT / 'tests/composable-counter' / COUNTER_FIXTURES[engine] / 'Cargo.toml'
        verify_counter_inputs(frozen)
        verify_counter_inputs({'receipts': receipts})
        check_counter_configuration(cwd, Path(env['CARGO_HOME']))
        command = minimal_compile_command(tools['cargo'], manifest, workload, target, protocol['target'])
        log = staging / 'commands' / f'{label}.txt'
        monitor.idle(f'compile:{label}', sustained=True)
        monitor.run(command, log, cwd, {**env, 'CARGO_TARGET_DIR': str(target)},
                    timeout=protocol['process_timeout_seconds'], counter_receipt=True)
        rows.append(minimal_compile_observation(log, command, cwd, target, item))
        if workload == 'release':
            binary = target / protocol['target'] / 'release/minimal-counter'
            digest = evidence._sha256(binary)
            run = staging / 'executions' / f'{label}.txt'
            _counter_command([str(binary)], run, env, timeout=protocol['execution_timeout_seconds'], cwd=cwd)
            validate_minimal_command(run, [str(binary)], cwd)
            if run.read_bytes() != protocol['expected_stdout'].encode():
                raise ValueError('minimal compile release output differs')
            verify_counter_inputs({'binary': {str(binary): digest}})
            binaries[str(binary)] = digest
        verify_counter_inputs(frozen)
        verify_counter_inputs({'receipts': receipts, 'binaries': binaries})
        for directory in ('commands', 'executions'):
            for path in (staging / directory).glob(f'{label}.*'):
                receipts[str(path)] = evidence._sha256(path)
    verify_counter_inputs({'receipts': receipts, 'binaries': binaries})
    return rows, receipts, binaries


def reload_minimal_compile(staging, protocol, inputs):
    """Reconstruct measured rows from raw counters and exact command identities."""
    cwd = Path(inputs['build_cwd'])
    rows = []
    for item in evidence.minimal_compile_plan(protocol):
        label = f"{item['block']:02d}-{item['workload']}-{item['engine']}"
        target = cwd / 'targets' / label
        manifest = Path(inputs['source_root']) / 'tests/composable-counter' / COUNTER_FIXTURES[item['engine']] / 'Cargo.toml'
        command = [inputs['tools']['cargo']['path'], 'check' if item['workload'] == 'check' else 'build',
                   '--manifest-path', str(manifest), '--locked', '--offline', '--bin', 'minimal-counter',
                   '--target', protocol['target'], '-j', '1',
                   *(['--release'] if item['workload'] == 'release' else [])]
        # Paths in receipts identify the original staging directory, including after publication.
        original_log = Path(inputs['capture_directory']) / 'commands' / f'{label}.txt'
        log = staging / 'commands' / f'{label}.txt'
        rows.append(minimal_compile_observation(log, command, cwd, target, item, original_log))
        if item['workload'] == 'release':
            binary = target / protocol['target'] / 'release/minimal-counter'
            run = staging / 'executions' / f'{label}.txt'
            validate_minimal_command(run, [str(binary)], cwd)
            if run.read_bytes() != protocol['expected_stdout'].encode():
                raise ValueError('minimal compile retained release output differs')
    return rows


def qualify_minimal_compile(output, staging):
    staging.parent.mkdir(parents=True, exist_ok=True)
    staging.mkdir()
    try:
        protocol = evidence.load_minimal_compile_protocol(MINIMAL_COMPILE_PROTOCOL)
        source, host = counter_source_identity(ROOT), canonical_host()
        (staging / 'protocol.json').write_bytes(MINIMAL_COMPILE_PROTOCOL.read_bytes())
        write_json(staging / 'plan.json', evidence.minimal_compile_plan(protocol))
        records = {str(staging / name): evidence._sha256(staging / name)
                   for name in ('protocol.json', 'plan.json')}
        with tempfile.TemporaryDirectory(prefix='rca-minimal-compile.', dir='/tmp') as temporary:
            cwd = Path(temporary)
            cargo_home = cwd / 'cargo-home'
            cargo_home.mkdir()
            env, tools = counter_environment(cwd, cargo_home)
            if any(env.get(key) != value for key, value in protocol['release_profile'].items()):
                raise ValueError('minimal compile release profile differs')
            env.update(protocol['check_profile'])
            tools = {**tools, 'time': '/usr/bin/time'}
            tool_hashes = {path: evidence._sha256(Path(path)) for path in tools.values()}
            for library in (Path(tools['rustc']).parent.parent / 'lib').glob('libLLVM*.dylib'):
                tool_hashes[str(library)] = evidence._sha256(library)
            frozen = dict(source=source['hashes'], tools=tool_hashes, records=records, preparation={})
            verify_counter_inputs(frozen)
            inputs = dict(source=source, source_root=str(ROOT), host=host, build_cwd=str(cwd),
                cargo_home=str(cargo_home), capture_directory=str(staging), environment=env,
                tools={name: dict(path=path, sha256=tool_hashes[path],
                    version=read([path, '--version'], env=env) if name != 'time' else 'Darwin /usr/bin/time -lp')
                    for name, path in tools.items()})
            write_json(staging / 'inputs.json', inputs)
            records[str(staging / 'inputs.json')] = evidence._sha256(staging / 'inputs.json')
            dependencies = {}
            for engine, fixture in COUNTER_FIXTURES.items():
                manifest = ROOT / 'tests/composable-counter' / fixture / 'Cargo.toml'
                base = [tools['cargo'], '--manifest-path', str(manifest)]
                fetch = staging / 'preparation' / f'{fixture}-fetch.txt'
                command = [base[0], 'fetch', *base[1:], '--locked', '--target', protocol['target']]
                _counter_command(command, fetch, env, cwd=cwd)
                validate_minimal_command(fetch, command, cwd)
                verify_counter_inputs(frozen)
                frozen['preparation'].update({str(path): evidence._sha256(path)
                    for path in fetch.parent.glob(f'{fixture}-fetch.*')})
                metadata = staging / 'preparation' / f'{fixture}-metadata.json'
                command = [base[0], 'metadata', *base[1:], '--locked', '--offline', '--format-version', '1',
                           '--filter-platform', protocol['target']]
                _counter_command(command, metadata, env, cwd=cwd)
                validate_minimal_command(metadata, command, cwd)
                verify_counter_inputs(frozen)
                document = json.loads(metadata.read_text())
                validate_minimal_metadata(document, engine, protocol)
                frozen['preparation'].update({str(path): evidence._sha256(path)
                    for path in metadata.parent.glob(f'{fixture}-metadata.*')})
                verify_counter_inputs({'dependencies': dependencies})
                dependencies.update(counter_dependency_identity(document, manifest.with_name('Cargo.lock'), cargo_home))
            write_json(staging / 'dependency-hashes.json', dependencies)
            records[str(staging / 'dependency-hashes.json')] = evidence._sha256(staging / 'dependency-hashes.json')
            frozen['dependencies'] = dependencies
            monitor = HostMonitor(staging / 'host', protocol)
            rows, receipts, binaries = capture_minimal_compile(staging, cwd, protocol, env, tools, monitor, frozen)
            frozen.update(receipts=receipts, binaries=binaries)
            result = evidence.evaluate_minimal_compile(rows, protocol)
            if evidence.evaluate_minimal_compile(reload_minimal_compile(staging, protocol, inputs), protocol) != result:
                raise ValueError('minimal compile reloaded analysis differs')
            write_json(staging / 'analysis.json', result)
            records[str(staging / 'analysis.json')] = evidence._sha256(staging / 'analysis.json')
            verify_counter_inputs(frozen)
            check_counter_configuration(cwd, cargo_home)
            if counter_source_identity(ROOT) != source or canonical_host()['identity'] != host['identity']:
                raise ValueError('minimal compile source or host changed')
            frozen['capture'] = {str(path): evidence._sha256(path) for path in staging.rglob('*') if path.is_file()}
            write_json(staging / 'capture-input-hashes.json', frozen)
            verify_counter_inputs(frozen)
        complete = dict(provenance_qualified=True, compile_capture_complete=True, timing_qualified=False,
                        qualified_comparison=False, regression_pass=False, recorded_at=now(),
                        sha256={str(path.relative_to(staging)): evidence._sha256(path)
                                for path in sorted(staging.rglob('*')) if path.is_file()})
        write_json(staging / 'complete.json', complete)
        publish(staging, output)
        return 0
    except BaseException as error:
        write_json(staging / 'failure.json', dict(recorded_at=now(), error=str(error),
            timing_qualified=False, qualified_comparison=False, regression_pass=False))
        raise


def minimal_macho_identity(path):
    """Read only the fixed Mach-O header; fat binaries are not this target."""
    with path.open("rb") as stream:
        header = stream.read(32)
    if len(header) != 32:
        raise ValueError("minimal application has a truncated Mach-O header")
    magic, cpu, _subtype, kind, *_ = struct.unpack("<8I", header)
    if (magic, cpu, kind) != (0xFEEDFACF, 0x100000C, 2):
        raise ValueError("minimal application is not an arm64 Mach-O executable")
    return "Mach-O 64-bit arm64 executable"


def validate_minimal_metadata(metadata, engine, protocol):
    packages = {package["id"]: package for package in metadata["packages"]}
    active = [packages[node["id"]] for node in metadata["resolve"]["nodes"]]
    if any(package["name"] == "dhat" for package in active):
        raise ValueError("minimal application activates allocation instrumentation")
    if any("allocation-measurement" in node["features"] for node in metadata["resolve"]["nodes"]):
        raise ValueError("minimal application activates measurement features")
    if engine in protocol["pins"]:
        sources = [package.get("source", "") for package in active if package["name"] == "composable"]
        pin = protocol["pins"][engine]
        expected = f"git+{protocol['repositories'][engine]}?rev={pin}#{pin}"
        if sources != [expected]:
            raise ValueError("minimal application legacy revision differs")


def validate_minimal_command(transcript, command, cwd):
    if json.loads(transcript.with_suffix(".command.json").read_text()) != {
            "command": command, "cwd": str(cwd)}:
        raise ValueError("minimal application command receipt differs")
    exit_record = json.loads(transcript.with_suffix(".exit.json").read_text())
    if exit_record != {"returncode": 0} or type(exit_record["returncode"]) is not int:
        raise ValueError("minimal application command failed")
    if not transcript.is_file() or not transcript.with_suffix(".stderr.txt").is_file():
        raise ValueError("minimal application transcript is missing")


def minimal_size_observation(directory, engine, build, protocol, strip_tool, cwd,
                             execution_directory=None):
    execution_directory = execution_directory or directory
    row = {"engine": engine, "build": build, "target": protocol["target"], "format": protocol["format"]}
    for phase in ("original", "stripped", "strip"):
        transcript = directory / f"{phase}.txt"
        command = ([strip_tool, *protocol["strip_arguments"], str(execution_directory / "stripped")]
                   if phase == "strip" else [str(execution_directory / phase)])
        validate_minimal_command(transcript, command, cwd)
        row[f"{phase}_exit"] = 0
        if phase != "strip":
            binary = directory / phase
            if binary.is_symlink() or not binary.is_file() or not os.access(binary, os.X_OK):
                raise ValueError("minimal application binary is missing or not executable")
            if minimal_macho_identity(binary) != protocol["format"]:
                raise ValueError("minimal application format differs")
            row[f"{phase}_bytes"] = binary.stat().st_size
            row[f"{phase}_sha256"] = evidence._sha256(binary)
            row[f"{phase}_stdout"] = transcript.read_bytes().decode("utf-8")
    return row


def load_minimal_size_publication(directory):
    """Replay retained consistency, not independently attest the recorded origins."""
    complete = json.loads((directory / "complete.json").read_text())
    if (complete.get("size_capture_complete") is not True
            or complete.get("timing_qualified") is not False
            or complete.get("qualified_comparison") is not False):
        raise ValueError("minimal size completion boundary differs")
    files = {str(path.relative_to(directory)): path for path in directory.rglob("*") if path.is_file()}
    files.pop("complete.json", None)
    required = {"protocol.json", "inputs.json", "analysis.json", "binaries.json",
                "dependency-hashes.json", "build-receipt-hashes.json", "capture-input-hashes.json"}
    if not required.issubset(files):
        raise ValueError("minimal size publication lacks provenance records")
    if set(files) != set(complete["sha256"]):
        raise ValueError("minimal size publication inventory differs")
    verify_counter_inputs({"publication": {str(files[name]): digest for name, digest in complete["sha256"].items()}})
    protocol = evidence.load_minimal_size_protocol(directory / "protocol.json")
    inputs = json.loads((directory / "inputs.json").read_text())
    if (inputs.get("target") != protocol["target"] or inputs.get("features") != protocol["features"]
            or inputs.get("release_environment") != protocol["profile"]
            or protocol["strip_version"] not in inputs["tools"]["strip"].get("version", "")):
        raise ValueError("minimal size published build inputs differ")
    captured = Path(inputs["capture_directory"])
    cwd = Path(inputs["build_cwd"])
    strip_tool = inputs["tools"]["strip"]["path"]
    if any(not path.is_absolute() or ".." in path.parts for path in (captured, cwd, Path(strip_tool))):
        raise ValueError("minimal size retained execution paths are invalid")
    rows = [minimal_size_observation(directory / "applications" / engine / build,
                                    engine, build, protocol, strip_tool, cwd,
                                    captured / "applications" / engine / build)
            for engine in protocol["engines"] for build in protocol["builds"]]
    result = evidence.evaluate_minimal_size(rows, protocol)
    validate_minimal_provenance(directory, captured, cwd, inputs, rows, protocol, complete["sha256"])
    if result != json.loads((directory / "analysis.json").read_text()):
        raise ValueError("minimal size published analysis differs from retained bytes")
    return result


def validate_minimal_provenance(directory, captured, cwd, inputs, rows, protocol, inventory):
    """Cross-check captured provenance records; external origins need separate audit."""
    import re

    def hashes(value):
        if (not isinstance(value, dict) or not value
                or any(not isinstance(path, str) or not Path(path).is_absolute()
                       or ".." in Path(path).parts or not isinstance(digest, str)
                       or not re.fullmatch(r"[0-9a-f]{64}", digest)
                       for path, digest in value.items())):
            raise ValueError("minimal size provenance hash identities are invalid")
        return value

    def staged_hashes(value):
        for path, digest in hashes(value).items():
            try:
                name = str(Path(path).relative_to(captured))
            except ValueError as error:
                raise ValueError("minimal size retained hash escapes capture") from error
            if inventory.get(name) != digest:
                raise ValueError("minimal size retained hashes disagree")

    try:
        source = inputs["source"]
        if not isinstance(source["revision"], str) or not re.fullmatch(r"[0-9a-f]{40}", source["revision"]):
            raise ValueError("minimal size source revision is invalid")
        source_hashes = hashes(source["hashes"])
        source_root = Path(inputs["source_root"])
        if not source_root.is_absolute() or any(not Path(path).is_relative_to(source_root) for path in source_hashes):
            raise ValueError("minimal size source paths differ")
        frozen = json.loads((directory / "capture-input-hashes.json").read_text())
        dependencies = hashes(json.loads((directory / "dependency-hashes.json").read_text()))
        build_receipts = json.loads((directory / "build-receipt-hashes.json").read_text())
        if frozen["source"] != source_hashes or frozen["dependencies"] != dependencies or frozen["build_receipts"] != build_receipts:
            raise ValueError("minimal size recorded identities disagree")
        tool_hashes = hashes(frozen["tools"])
        versions = {"cargo": "cargo 1.98.0 (797e8a9bc 2026-08-05)",
                    "rustc": "rustc 1.98.0 (88d9e12ae 2026-08-18)",
                    "rustdoc": "rustdoc 1.98.0 (88d9e12ae 2026-08-18)"}
        if set(inputs["tools"]) != {*versions, "strip"}:
            raise ValueError("minimal size tool inventory differs")
        for name, tool in inputs["tools"].items():
            if tool_hashes.get(tool["path"]) != tool["sha256"]:
                raise ValueError("minimal size tool hashes disagree")
            if name in versions and tool["version"] != versions[name]:
                raise ValueError("minimal size tool versions differ")
        staged_hashes(build_receipts)
        staged_hashes(frozen["applications"])
        staged_hashes(frozen["capture"])
        expected_build_files = {str(captured / name) for name in inventory if name.startswith("builds/")}
        expected_app_files = {str(captured / name) for name in inventory if name.startswith("applications/")}
        if set(build_receipts) != expected_build_files or set(frozen["applications"]) != expected_app_files:
            raise ValueError("minimal size receipt hash coverage differs")
        expected_capture = {str(captured / name) for name in inventory if name != "capture-input-hashes.json"}
        if set(frozen["capture"]) != expected_capture:
            raise ValueError("minimal size capture hash coverage differs")
        binaries = json.loads((directory / "binaries.json").read_text())
        if set(binaries) != set(protocol["engines"]):
            raise ValueError("minimal size build inventory differs")
        binary_hashes = {}
        for engine, fixture in COUNTER_FIXTURES.items():
            manifest = source_root / "tests/composable-counter" / fixture / "Cargo.toml"
            if str(manifest) not in source_hashes or str(manifest.with_name("Cargo.lock")) not in source_hashes:
                raise ValueError("minimal size manifest or lock identity missing")
            base = [inputs["tools"]["cargo"]["path"], "--manifest-path", str(manifest)]
            metadata = directory / "builds" / f"{fixture}-metadata.json"
            validate_minimal_command(metadata, [base[0], "metadata", *base[1:], "--locked", "--offline",
                "--format-version", "1", "--filter-platform", protocol["target"]], cwd)
            validate_minimal_metadata(json.loads(metadata.read_text()), engine, protocol)
            validate_minimal_command(directory / "builds" / f"{fixture}-fetch.txt",
                [base[0], "fetch", *base[1:], "--locked", "--target", protocol["target"]], cwd)
            if set(binaries[engine]) != set(protocol["builds"]):
                raise ValueError("minimal size build arms differ")
            for build in protocol["builds"]:
                binary = binaries[engine][build]
                expected = cwd / "targets" / f"{fixture}-{build}" / protocol["target"] / "release/minimal-counter"
                row = next(row for row in rows if (row["engine"], row["build"]) == (engine, build))
                if binary != {"path": str(expected), "sha256": row["original_sha256"]}:
                    raise ValueError("minimal size original differs from built artifact")
                binary_hashes[str(expected)] = binary["sha256"]
                validate_minimal_command(directory / "builds" / f"{fixture}-{build}.txt",
                    [base[0], "build", *base[1:], "--locked", "--offline", "--release",
                     "--bin", "minimal-counter", "--target", protocol["target"], "-j", "1"], cwd)
        if frozen["binaries"] != binary_hashes:
            raise ValueError("minimal size binary identities disagree")
    except (KeyError, TypeError, AttributeError) as error:
        raise ValueError("minimal size provenance records are malformed") from error


def capture_minimal_sizes(staging, binaries, protocol, env, strip_tool, cwd):
    rows = []
    captured = {}
    for engine in protocol["engines"]:
        for build in protocol["builds"]:
            verify_counter_inputs({"earlier_applications": captured})
            source = binaries[engine][build]
            verify_counter_inputs({"binary": {source["path"]: source["sha256"]}})
            directory = staging / "applications" / engine / build
            directory.mkdir(parents=True)
            original, stripped = directory / "original", directory / "stripped"
            shutil.copy2(source["path"], original)
            original_hash = evidence._sha256(original)
            if original_hash != source["sha256"]:
                raise ValueError("minimal application copy changed")
            minimal_macho_identity(original)
            _counter_command([str(original)], directory / "original.txt", env,
                             timeout=protocol["process_timeout_seconds"], cwd=cwd)
            validate_minimal_command(directory / "original.txt", [str(original)], cwd)
            if (directory / "original.txt").read_bytes() != protocol["expected_stdout"].encode():
                raise ValueError("minimal application original output differs")
            verify_counter_inputs({"original": {str(original): original_hash}})
            captured.update({str(path): evidence._sha256(path) for path in directory.iterdir() if path.is_file()})
            shutil.copy2(original, stripped)
            _counter_command([strip_tool, *protocol["strip_arguments"], str(stripped)],
                             directory / "strip.txt", env,
                             timeout=protocol["process_timeout_seconds"], cwd=cwd)
            verify_counter_inputs({"original_execution": captured})
            validate_minimal_command(directory / "strip.txt",
                                     [strip_tool, *protocol["strip_arguments"], str(stripped)], cwd)
            stripped_hash = evidence._sha256(stripped)
            minimal_macho_identity(stripped)
            captured.update({str(path): evidence._sha256(path) for path in directory.iterdir() if path.is_file()})
            _counter_command([str(stripped)], directory / "stripped.txt", env,
                             timeout=protocol["process_timeout_seconds"], cwd=cwd)
            verify_counter_inputs({"pre_execution": captured})
            verify_counter_inputs({"applications": {str(original): original_hash,
                                                    str(stripped): stripped_hash}})
            row = minimal_size_observation(directory, engine, build, protocol, strip_tool, cwd)
            rows.append(row)
            # Reject the first failed work result rather than completing more builds/runs.
            if row["original_stdout"] != protocol["expected_stdout"] or row["stripped_stdout"] != protocol["expected_stdout"]:
                raise ValueError("minimal application output differs")
            verify_counter_inputs({"earlier_applications": captured})
            captured.update({str(path): evidence._sha256(path) for path in directory.iterdir() if path.is_file()})
            if minimal_size_observation(directory, engine, build, protocol, strip_tool, cwd) != row:
                raise ValueError("minimal application observation changed during admission")
    verify_counter_inputs({"applications": captured})
    reloaded = [minimal_size_observation(staging / "applications" / engine / build,
                                        engine, build, protocol, strip_tool, cwd)
                for engine in protocol["engines"] for build in protocol["builds"]]
    if reloaded != rows:
        raise ValueError("minimal application observations changed")
    return rows, captured


def qualify_minimal_size(output, staging):
    staging.parent.mkdir(parents=True, exist_ok=True)
    staging.mkdir()
    try:
        protocol = evidence.load_minimal_size_protocol(MINIMAL_SIZE_PROTOCOL)
        source, host = counter_source_identity(ROOT), canonical_host()
        (staging / "protocol.json").write_bytes(MINIMAL_SIZE_PROTOCOL.read_bytes())
        with tempfile.TemporaryDirectory(prefix="rca-minimal-size.", dir="/tmp") as temporary:
            cwd = Path(temporary)
            cargo_home = cwd / "cargo-home"
            cargo_home.mkdir()
            env, tools = counter_environment(cwd, cargo_home)
            target_libdir = Path(read([tools["rustc"], "--print", "target-libdir"], env=env))
            strip_tool = str((target_libdir.parent / "bin/rust-objcopy").resolve())
            tools = {**tools, "strip": strip_tool}
            versions = {name: read([path, "--version"], env=env) for name, path in tools.items()}
            if protocol["strip_version"] not in versions["strip"]:
                raise ValueError("minimal size needs the pinned Rust stripping tool")
            tool_hashes = {path: evidence._sha256(Path(path)) for path in tools.values()}
            # The stripping executable loads the pinned LLVM dynamic library.
            for library in (Path(tools["rustc"]).parent.parent / "lib").glob("libLLVM*.dylib"):
                tool_hashes[str(library)] = evidence._sha256(library)
            frozen = {"source": source["hashes"], "tools": tool_hashes}
            write_json(staging / "inputs.json", {"source": source, "host": host,
                       "source_root": str(ROOT),
                       "tools": {name: {"path": path, "sha256": tool_hashes[path], "version": versions[name]}
                                 for name, path in tools.items()}, "target": protocol["target"],
                       "features": [], "release_environment": COUNTER_RELEASE_ENV,
                       "build_cwd": str(cwd), "cargo_home": str(cargo_home),
                       "capture_directory": str(staging)})
            binaries = build_counter_binaries(staging, cwd / "targets", protocol, env, tools, minimal_size=True)
            write_json(staging / "binaries.json", binaries)
            frozen["dependencies"] = json.loads((staging / "dependency-hashes.json").read_text())
            frozen["build_receipts"] = json.loads((staging / "build-receipt-hashes.json").read_text())
            frozen["binaries"] = {item["path"]: item["sha256"] for builds in binaries.values() for item in builds.values()}
            verify_counter_inputs(frozen)
            rows, captured = capture_minimal_sizes(staging, binaries, protocol, env, strip_tool, cwd)
            frozen["applications"] = captured
            verify_counter_inputs(frozen)
            result = evidence.evaluate_minimal_size(rows, protocol)
            write_json(staging / "analysis.json", result)
            frozen["capture"] = {str(path): evidence._sha256(path) for path in staging.rglob("*") if path.is_file()}
            write_json(staging / "capture-input-hashes.json", frozen)
            verify_counter_inputs(frozen)
            check_counter_configuration(cwd, cargo_home)
            if counter_source_identity(ROOT) != source or canonical_host()["identity"] != host["identity"]:
                raise ValueError("minimal size source or host changed")
        complete = {"provenance_qualified": True, "size_capture_complete": True,
                    "timing_qualified": False, "qualified_comparison": False, "recorded_at": now(),
                    "sha256": {str(path.relative_to(staging)): evidence._sha256(path)
                               for path in sorted(staging.rglob("*")) if path.is_file()}}
        write_json(staging / "complete.json", complete)
        publish(staging, output)
        return 0
    except BaseException as error:
        write_json(staging / "failure.json", {"recorded_at": now(), "error": str(error),
                                             "timing_qualified": False, "qualified_comparison": False})
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode",
        choices=(
            "candidate-reference",
            "reference-reference-aa",
            "counter-cohorts",
            "counter-lifecycle-cohorts",
            "counter-allocation-cohorts",
            "ready-future-allocation-cohorts",
            "common-outcome-allocation-cohorts",
            "minimal-application-size",
            "minimal-application-compile",
        ),
        default="candidate-reference",
    )
    parser.add_argument("--check-prerequisites", action="store_true")
    parser.add_argument("--check-staged-source")
    parser.add_argument("--aa-evidence")
    parser.add_argument("--check-publication")
    parser.add_argument("--check-lock", nargs=2, metavar=("PATH", "SECONDS"))
    parser.add_argument("output", nargs="?")
    parser.add_argument("reference", nargs="?")
    args = parser.parse_args()
    try:
        if args.check_staged_source:
            print(json.dumps(validate_staged_source(args.check_staged_source)))
            return 0
        if args.check_prerequisites:
            tool_environment()
            print("cargo-bloat 0.12.1 is available through pinned Cargo")
            return 0
        if args.check_lock:
            seconds = float(args.check_lock[1])
            if not 0 <= seconds <= 10:
                raise ValueError(
                    "lock smoke duration must be between zero and ten seconds"
                )
            with machine_lock(Path(args.check_lock[0])):
                print("lock acquired", flush=True)
                time.sleep(seconds)
            return 0
        if args.check_publication:
            output, staging = output_paths(args.check_publication)
            staging.parent.mkdir(parents=True, exist_ok=True)
            staging.mkdir()
            (staging / "publication-smoke.txt").write_text("complete\n")
            publish(staging, output)
            return 0
        if args.mode in ("counter-cohorts", "counter-lifecycle-cohorts", "counter-allocation-cohorts",
                         "ready-future-allocation-cohorts", "common-outcome-allocation-cohorts",
                         "minimal-application-size", "minimal-application-compile"):
            if not args.output or args.reference or args.aa_evidence:
                raise ValueError("counter capture needs one output and no external reference or A/A evidence")
            output, staging = output_paths(args.output)
            with machine_lock(LOCK):
                if args.mode == 'minimal-application-compile':
                    return qualify_minimal_compile(output, staging)
                if args.mode == "minimal-application-size":
                    return qualify_minimal_size(output, staging)
                if args.mode == "ready-future-allocation-cohorts":
                    return qualify_counter(output, staging, allocations=True, ready_futures=True)
                if args.mode == "common-outcome-allocation-cohorts":
                    return qualify_counter(output, staging, allocations=True, common_outcome=True)
                if args.mode == "counter-allocation-cohorts":
                    return qualify_counter(output, staging, allocations=True)
                if args.mode == "counter-lifecycle-cohorts":
                    return qualify_counter(output, staging, lifecycle=True)
                return qualify_counter(output, staging)
        if not args.output or not args.reference:
            parser.error("output and reference checkout are required")
        if args.mode != "candidate-reference" or args.aa_evidence:
            raise ValueError("composition qualification captures both modes together; external A/A evidence is not accepted")
        output, staging = output_paths(args.output)
        with machine_lock(LOCK):
            return qualify(
                output, staging, Path(args.reference), args.mode, args.aa_evidence
            )
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(
            f"composition qualification: {error}; incomplete evidence is retained",
            file=sys.stderr,
        )
        return 1


if __name__ == "__main__":

    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    sys.exit(main())
