#!/usr/bin/env python3
"""Run the frozen composition protocols on their separate native hosts."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import ctypes
from datetime import datetime, timezone
import hashlib
import json
import io
import importlib.util
import math
import ntpath
import os
import platform
from pathlib import Path, PureWindowsPath
import signal
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

sys.dont_write_bytecode = True
import composition_performance as evidence  # noqa: E402 -- suppress bytecode before local imports
import native_release_reference  # noqa: E402
import windows_strip  # noqa: E402


ROOT = Path(__file__).resolve().parents[1]
POLICIES = ROOT / "benches/performance"
CURRENT_PROTOCOL = POLICIES / "composition-macos.json"
WINDOWS_PROTOCOL = POLICIES / "composition-windows.json"
WINDOWS_COMPLETE_POLICY = POLICIES / "composition-windows-complete.json"
BUDGET = POLICIES / "composition-budgets.json"
EVIDENCE_ROOT = ROOT / "target/qualification-evidence/composition"
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


def protocol_path(protocol):
    """Return the policy that the measured host must use."""
    if evidence.is_windows_protocol(protocol):
        return WINDOWS_PROTOCOL
    if evidence.is_current_mac_protocol(protocol):
        return CURRENT_PROTOCOL
    raise ValueError("unsupported composition protocol")


def receipt_files(protocol, *, windows_complete=False):
    """Windows paired evidence cannot include a Darwin absolute-gate result."""
    files = tuple(name for name in AA_RECEIPT_FILES
                  if name != "absolute.json" or not evidence.is_windows_protocol(protocol))
    return (*files, "absolute-windows.json") if windows_complete else files


def windows_objcopy(tools):
    """Use only the LLVM strip tool shipped with the pinned Windows compiler."""
    sysroot = Path(read([tools["rustc"], "--print", "sysroot"]))
    tool = sysroot / "lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-objcopy.exe"
    if not sysroot.is_absolute() or tool.is_symlink() or not tool.is_file():
        raise ValueError("pinned Windows llvm-objcopy is missing")
    return str(tool)


def windows_provider():
    """Load the existing Windows counter definitions without running its CLI."""
    path = ROOT / "scripts/capture-windows-performance.py"
    spec = importlib.util.spec_from_file_location("rca_windows_counters", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

# Only consumer workloads and their test-only support can replace archived inputs.
COMMON_HARNESS_FILES = {
    path: path for path in (
        "crates/rca-core/benches/composition.rs",
        "crates/rca-core/benches/composition/fixtures.rs",
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
    "crates/rca-core/tests/composition_qualification/main.rs": "scripts/fixtures/composition-zero.rs",
    "crates/rca-core/tests/composition_qualification/composition-zero-runner.rs": "crates/rca-core/src/allocation_test/runner.rs",
}


def source_hashes(root):
    """Identify every staged input; Cargo outputs live outside this source directory."""
    return {path.relative_to(root).as_posix(): evidence._sha256(path)
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
    for name, row in files.items():
        destination = product / name
        if destination.is_symlink() or not destination.resolve().is_relative_to(product.resolve()):
            raise ValueError("common workload destination escapes the staged source")
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(controller / row["source"], destination)


def stage_source(source, identity, destination, protocol):
    """Archive one exact revision and add only the frozen consumer harness and adapters."""
    destination.mkdir()
    archive = subprocess.check_output(["git", "--no-replace-objects", "archive", "--format=tar", identity["revision"]], cwd=source)
    with tarfile.open(fileobj=io.BytesIO(archive)) as stream:
        stream.extractall(destination, filter="data")
    original = source_hashes(destination)
    files = protocol["common_workloads"]["files"]
    apply_common_harness(ROOT, destination, files)
    manifest = destination / "crates/rca-core/Cargo.toml"
    with manifest.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write('\n[[test]]\nname = "composition_zero"\npath = "tests/composition_qualification/main.rs"\n'
                     'harness = false\nrequired-features = ["allocation-measurement"]\n')
    return dict(root=str(destination), revision=identity["revision"], tree=identity["tree"],
                adapters=["external-zero-host-v1"], common_workloads=files, original=original,
                effective=source_hashes(destination))


def validate_staged_source(receipt_path):
    """Reject changed staged inputs before capture and before accepting its evidence."""
    receipt = json.loads(Path(receipt_path).read_text(encoding="utf-8"))
    root = Path(receipt["root"])
    changed = {name for name in receipt["original"].keys() | receipt["effective"].keys()
               if receipt["original"].get(name) != receipt["effective"].get(name)}
    if changed - (COMMON_HARNESS_FILES.keys() | {"crates/rca-core/Cargo.toml"}):
        raise ValueError("staged source replaced product implementation")
    if source_hashes(root) != receipt["effective"]:
        raise ValueError("staged composition source changed")
    return receipt


def read(command, cwd=None, env=None, timeout=30):
    return subprocess.check_output(command, cwd=cwd, env=env, text=True,
                                   encoding="utf-8", timeout=timeout).strip()


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
        if value.startswith(str(staging) + os.sep):
            return Path(value).relative_to(staging).as_posix()
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
    """Publish on the same filesystem without replacing existing evidence."""
    if sys.platform == "win32":
        # Windows rename fails when the target already exists.
        os.rename(staged, target)
        return
    if sys.platform != "darwin":
        raise OSError("atomic publication requires Darwin or Windows")
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


def validate_aa_evidence(value, protocol, controller, host, *, capture=None,
                         windows_complete=None):
    path = Path(value).absolute().resolve()
    windows = evidence.is_windows_protocol(protocol)
    if capture is None:
        raise ValueError("A/A evidence needs its contemporaneous capture")
    if path != (capture / "reference-reference-aa").resolve():
        raise ValueError("A/A evidence is outside its contemporaneous capture")
    validate_interleaved_capture(capture, protocol)
    try:
        receipts = {
            name: json.loads((path / name).read_text(encoding="utf-8"))
            for name in receipt_files(protocol, windows_complete=windows_complete is not None)
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
    absolute = receipts.get("absolute.json")
    complete = receipts["complete.json"]
    if not all(
        isinstance(item, dict) for item in (inputs, comparison, complete)
    ):
        raise ValueError("A/A evidence receipt is malformed")
    if not all(
        isinstance(inputs.get(key), dict)
        for key in ("controller", "host", "subject", "control")
    ):
        raise ValueError("A/A evidence input identity is malformed")
    comparison_rows = comparison.get("rows")
    absolute_rows = None if windows else absolute.get("rows") if isinstance(absolute, dict) else None
    if not all(
        isinstance(rows, list) and all(isinstance(row, dict) for row in rows)
        for rows in ((comparison_rows,) if windows else (comparison_rows, absolute_rows))
    ):
        raise ValueError("A/A evidence gate rows are malformed")
    expected_relative = sum(
        row["scenario"].startswith(("runtime/", "compile/"))
        for row in evidence.validate_budget_document(BUDGET)["rows"]
    )
    if (
        metadata["mode"] != "reference-reference-aa"
        or metadata["qualification_script_sha256"] != evidence._sha256(Path(__file__))
        or (evidence.is_current_mac_protocol(protocol)
            and metadata.get("memory_watchdog_sha256")
            != evidence._sha256(ROOT / "scripts/guard_cargo_memory.py"))
        or metadata["evaluator_script_sha256"]
        != evidence._sha256(ROOT / "scripts/composition_performance.py")
        or inputs.get("mode") != "reference-reference-aa"
        or inputs.get("controller", {}).get("revision") != controller["revision"]
        or inputs.get("host", {}).get("identity") != host["identity"]
        or any(metadata[key] != host["identity"].get(key) for key in ("os_version", "os_build"))
        or inputs.get("subject", {}).get("revision") != evidence.expected_reference_revision(protocol)
        or inputs.get("control", {}).get("revision") != evidence.expected_reference_revision(protocol)
        or complete.get("passed") is not True
        or comparison.get("mode") != "reference-reference-aa"
        or comparison.get("passed") is not True
        or comparison.get("failed_gates") != []
        or comparison.get("metadata") != metadata
        or len(comparison_rows) != expected_relative
        or not all(row.get("passed") is True for row in comparison_rows)
    ):
        raise ValueError("A/A evidence does not prove passing same-host reference gates")
    if windows:
        expected_scope = "windows-complete" if windows_complete is not None else "windows-paired-only"
        if (complete.get("qualification_scope") != expected_scope
                or complete.get("mac_absolute_qualified") is not False
                or (path / "absolute.json").exists() or (path / "absolute-raw").exists()):
            raise ValueError("Windows A/A evidence cannot certify Darwin absolute gates")
    elif (absolute.get("mode") != "reference-reference-aa"
            or absolute.get("subject_revision") != evidence.expected_reference_revision(protocol)
            or absolute.get("reference_only") is not True or absolute.get("passed") is not True
            or absolute.get("failed_budgets") != [] or len(absolute_rows) != 33
            or not all(row.get("passed") is True for row in absolute_rows)):
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
        path / "diagnostics", ("reference-b", "reference-a"), protocol,
        metadata.get("memory_watchdog_sha256"),
        metadata.get("memory_limit_bytes"),
    )
    if windows:
        if windows_complete is not None:
            import composition_windows_complete as complete_evidence
            baseline_path = capture / "Windows-reference-baseline.json"
            if (baseline_path.is_symlink()
                    or evidence._sha256(baseline_path) != complete.get("windows_reference_baseline_sha256")
                    or evidence._sha256(path / "absolute-windows.json") != complete.get("absolute_windows_sha256")):
                raise ValueError("Windows A/A baseline or report hash differs")
            baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
            raw = json.loads((path / "absolute-windows-raw/observation.json").read_text(encoding="utf-8"))
            expected_baseline = complete_evidence.seal_reference(
                windows_complete, BUDGET, subject_report, raw)
            if baseline != expected_baseline:
                raise ValueError("Windows A/A baseline disagrees with raw measurements")
            checked = complete_evidence.evaluate(windows_complete, BUDGET, subject_report, raw, baseline)
            if not checked["passed"] or report_paths(checked, path) != receipts["absolute-windows.json"]:
                raise ValueError("Windows A/A absolute gates disagree with raw measurements")
        return {"path": str(path),
                "sha256": {name: evidence._sha256(path / name) for name in
                           receipt_files(protocol, windows_complete=windows_complete is not None)},
                **({"windows_reference_baseline_sha256": evidence._sha256(baseline_path)}
                   if windows_complete is not None else {})}
    checked_absolute = evidence.evaluate_absolute(
        BUDGET,
        path / "absolute-raw",
        evidence.expected_reference_revision(protocol),
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
    binary = root / "target/performance-tools/bin/cargo-bloat"
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
        name: read(["rustup", "which", "--toolchain", "1.100.0", name])
        for name in ("cargo", "rustc", "rustdoc")
    }
    versions = {name: read([path, "--version"]) for name, path in tools.items()}
    if (
        versions
        != evidence.RUST100_TOOL_VERSIONS
        or len({str(Path(path).parent) for path in tools.values()}) != 1
    ):
        raise ValueError("exact Cargo, rustc, and rustdoc 1.100.0 required")
    env = dict(os.environ)
    env.update({name.upper(): path for name, path in tools.items()})
    env.update(
        PATH=(f"{Path(tools['cargo']).parent};{os.environ.get('PATH', '')}"
              if sys.platform == "win32" else
              f"{ROOT}/target/performance-tools/bin:{Path(tools['cargo']).parent}:/usr/bin:/bin:/usr/sbin:/sbin:{os.environ.get('PATH', '')}"),
        RUSTC_WRAPPER="",
        RUSTC_WORKSPACE_WRAPPER="",
        CARGO_BUILD_JOBS="1",
    )
    if include_capture_tool:
        tools["python"] = str(Path(sys.executable).resolve(strict=True))
        env["RCA_COMPOSITION_PYTHON"] = tools["python"]
        check_capture_tool(ROOT, tools["cargo"], env)
    return env, tools


def source_identity(root, reference=False):
    revision = read(["git", "--no-replace-objects", "rev-parse", "HEAD"], cwd=root)
    if read(["git", "--no-replace-objects", "status", "--porcelain=v1", "--untracked-files=all"], cwd=root):
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
    tree = read(["git", "--no-replace-objects", "rev-parse", "HEAD^{tree}"], cwd=root)
    if reference:
        expected = native_release_reference.composition_reference()
        if (revision, tree, hashes["Cargo.lock"]) != (expected["revision"], expected["tree"], expected["lock_sha256"]):
            raise ValueError("checkout differs from the trusted performance reference")
    return {"root": str(root), "revision": revision, "tree": tree, "hashes": hashes}


def rank_revisions(controller, subject, expected_controller, expected_subject):
    """Compare independently supplied full commits, never identity claims from the result."""
    for name, identity, expected in (("controller", controller, expected_controller),
                                     ("measured subject", subject, expected_subject)):
        if (not isinstance(expected, str) or not evidence._REVISION.fullmatch(expected)
                or not isinstance(identity, dict) or identity.get("revision") != expected):
            raise ValueError(f"missing or mismatched expected {name} revision")
    return {"controller_revision": expected_controller, "measured_subject_revision": expected_subject}


def rank_subject(checkout, controller, expected_controller, expected_subject):
    """An optional product checkout supplies only product source, never the controller."""
    if checkout is None and expected_controller is None and expected_subject is None:
        return controller
    subject = controller
    if checkout is not None:
        root = Path(checkout).resolve(strict=True)
        if Path(read(["git", "rev-parse", "--show-toplevel"], cwd=root)).resolve() != root:
            raise ValueError("--subject must name the clean product checkout root")
        subject = source_identity(root)
    rank_revisions(controller, subject, expected_controller, expected_subject)
    return subject


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


WINDOWS_HOSTNAME_SHA256 = "6c1f7706bd2507833c76d36ad2109bf97babf9385f20ee6b889b7e4e3775d4d1"


def windows_host():
    """Check the pinned runner without publishing its local name."""
    values = windows_provider().host_metadata()
    version = platform.version()
    hostname = values.get("hostname", "").casefold().encode("utf-8")
    if (hashlib.sha256(hostname).hexdigest() != WINDOWS_HOSTNAME_SHA256
            or values.get("logical_cpus") != 24
            or values.get("machine_model", "").casefold() != "amd64"
            or "AMD Ryzen 9 3900X" not in values.get("cpu", "")
            or not version or len(version.split(".")) != 3):
        raise ValueError("host does not match the Windows composition runner")
    return {"identity": {**values, "platform": "windows", "target": "x86_64-pc-windows-msvc",
                         "os_version": version, "os_build": version.rsplit(".", 1)[-1]}}


def check_host_unchanged(before, monitor):
    after = windows_host() if before["identity"].get("platform") == "windows" else canonical_host()
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


def guarded_mac_command(command, destination, *, cwd, env, timeout, stdout, stderr,
                        observe=None, observe_interval=1, memory_limit_bytes=8 * 1024**3):
    """Keep one owner for the measured group and retain its memory-safety receipt."""
    # Keep POSIX-only imports out of the Windows controller.
    import guard_cargo_memory as guard

    evidence.validate_mac_memory_limit(memory_limit_bytes)
    if evidence._sha256(Path(guard.__file__)) != guard.SOURCE_SHA256:
        raise ValueError("the loaded memory watchdog source changed")
    try:
        probe = guard.MacProbe()
    except Exception as error:
        receipt = dict(started=False, reason="setup-error", returncode=125, cleanup_complete=True,
                       error=str(error), memory_watchdog_sha256=guard.SOURCE_SHA256)
    else:
        receipt = guard.run_guarded(
            command, cwd=cwd, env=env, probe=probe, timeout=timeout,
            stdout=stdout, stderr=stderr, observe=observe, observe_interval=observe_interval,
            memory_limit=memory_limit_bytes,
        )
    if evidence._sha256(Path(guard.__file__)) != guard.SOURCE_SHA256:
        receipt.update(reason="watchdog-source-changed", returncode=125)
    write_json(destination.with_suffix(".memory.json"), receipt)
    write_json(destination.with_suffix(".exit.json"),
               {"returncode": receipt.get("command_returncode")})
    return receipt


def require_guarded_success(receipt, command, timeout):
    """Do not turn a watchdog stop or a failed telemetry sample into a measurement."""
    if receipt["reason"] == "timeout":
        raise subprocess.TimeoutExpired(command, timeout)
    if (receipt["reason"] != "command-exited" or not receipt["cleanup_complete"]
            or not receipt["descendants_complete"]):
        raise ValueError(f"memory watchdog stopped qualification: {receipt['reason']}: "
                         f"{receipt.get('error', '')}")
    if receipt["returncode"]:
        raise subprocess.CalledProcessError(receipt["returncode"], command)


class HostMonitor:
    def __init__(self, directory, protocol, *, memory_limit_bytes=8 * 1024**3):
        self.directory = directory
        self.protocol = protocol
        self.memory_limit_bytes = evidence.validate_mac_memory_limit(memory_limit_bytes)
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
        deadline = time.monotonic() + 30
        if self.process_deadline is not None:
            deadline = min(deadline, self.process_deadline)

        def probe(command):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise subprocess.TimeoutExpired(command, 0)
            return read(command, timeout=remaining)

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
            self.process_deadline = None if timeout is None else time.monotonic() + timeout
            try:
                receipt = guarded_mac_command(
                    executed, destination, cwd=cwd, env=env, timeout=timeout,
                    stdout=transcript, stderr=subprocess.STDOUT,
                    observe=lambda: self.sample(f"during:{destination.name}", active=True),
                    observe_interval=self.protocol["quiescence"]["process_monitor_interval_seconds"],
                    memory_limit_bytes=self.memory_limit_bytes,
                )
            finally:
                self.process_deadline = None
        self.event("processes.jsonl", {
            "phase": "finish", "destination": str(destination),
            "returncode": receipt.get("command_returncode"), "watchdog_reason": receipt["reason"],
        })
        if receipt["reason"] == "command-exited":
            self.sample(f"after:{destination.name}")
        require_guarded_success(receipt, command, timeout)
        if timed:
            write_json(
                destination.with_suffix(".resources.json"),
                evidence.parse_darwin_time(counter_path.read_text()),
            )


def check_windows_compiler_telemetry(protocol):
    """Find the telemetry tool and check its enrolled path and image before cleanup."""
    identity = protocol["compiler_telemetry"]
    install = Path(os.environ.get("VSINSTALLDIR", ""))
    if not install.is_absolute():
        raise ValueError("set VSINSTALLDIR to the enrolled Visual Studio installation")
    image = install.joinpath(*PureWindowsPath(identity["path_suffix"]).parts)
    linker = shutil.which("link.exe")
    if (not linker or Path(linker).resolve() != image.with_name("link.exe")
            or image.resolve() != image or not image.is_file() or evidence._sha256(image) != identity["sha256"]):
        raise ValueError("the pinned compiler telemetry image is missing or changed")
    return {"path": str(image), "sha256": identity["sha256"]}


class WindowsMonitor(HostMonitor):
    """Record owned process trees without rejecting background desktop load."""

    def __init__(self, directory, protocol, *, cargo=None, compiler_telemetry=None, provider=None, run_process=None,
                 complete=False, experimental_rank=False):
        super().__init__(directory, protocol)
        if run_process is None:
            from windows_process import run_process
        self.run_process = run_process
        self.provider = windows_provider().WindowsCounters() if provider is None else provider
        self.cargo = cargo
        self.compiler_telemetry = compiler_telemetry
        self.complete = complete
        self.experimental_rank = experimental_rank

    def idle(self, phase, sustained=False):
        self.event("telemetry.jsonl", {"phase": phase, "admission": "recorded-background-load"})

    def run(self, command, destination, cwd, env, timed=True, timeout=None, counter_receipt=False):
        if counter_receipt:
            raise ValueError("Darwin counter receipts are not supported by the Windows paired runner")
        destination.parent.mkdir(parents=True, exist_ok=True)
        executed = list(map(str, command))
        pinned_cargo = (self.cargo is not None and len(executed) > 1
                        and ntpath.normcase(ntpath.normpath(executed[0]))
                        == ntpath.normcase(ntpath.normpath(self.cargo)))
        cargo_arguments = executed[2:]
        if "--" in cargo_arguments:
            cargo_arguments = cargo_arguments[:cargo_arguments.index("--")]
        compiler = pinned_cargo and (
            executed[1] == "check"
            or (executed[1] == "bench" and "--no-run" in cargo_arguments)
            or (self.complete and (executed[1] == "build"
                or (executed[1] == "test" and "--no-run" in cargo_arguments))))
        divan_timing = not pinned_cargo and (
            executed[1:] == ["--bench"] or executed[1:3] == ["--bench", "--exact"])
        primary_thread_affinity = (self.protocol.get("method_version") == evidence.WINDOWS_V6_METHOD
                                   and divan_timing)

        timeout = self.protocol["process_timeout_seconds"] if timeout is None else timeout
        memory_limit = self.protocol["process_memory_limit_bytes"]
        cold = self.experimental_rank and compiler and executed[1] == "check"
        if cold and (not env.get("CARGO_BUILD_BUILD_DIR") or env["CARGO_BUILD_BUILD_DIR"] != env.get("CARGO_TARGET_DIR")):
            raise ValueError("rank cold build directory must equal its fresh target")
        self.event("processes.jsonl", {
            "phase": "start", "command": executed, "cwd": str(cwd),
            "destination": str(destination), "target": env.get("CARGO_TARGET_DIR"),
            **({"build_dir": env["CARGO_BUILD_BUILD_DIR"]} if cold else {}),
            "incremental": env.get("CARGO_INCREMENTAL"), "timeout_seconds": timeout,
            **({"allocation_scenario": env["RCA_COMPOSITION_ALLOCATION_SCENARIO"],
                "allocation_samples": env["RCA_COMPOSITION_ALLOCATION_SAMPLES"]}
               if "RCA_COMPOSITION_ALLOCATION_SCENARIO" in env else {}),
            "job_memory_limit_bytes": memory_limit,
            "command_kind": "compile" if compiler else "runtime",
            "compiler_telemetry": self.protocol.get("compiler_telemetry") if compiler else None,
        })
        result = None
        receipt = None
        failure = {}
        try:
            telemetry = check_windows_compiler_telemetry(self.protocol) if compiler else None
            if compiler:
                if self.compiler_telemetry is None:
                    self.compiler_telemetry = telemetry["path"]
                elif telemetry["path"] != self.compiler_telemetry:
                    raise ValueError("compiler telemetry image differs from the enrolled path")
            with destination.open("xb") as transcript:
                result = self.run_process(executed, transcript, str(cwd), env, timeout, self.provider,
                                          memory_limit_bytes=memory_limit,
                                          **({"primary_thread_affinity_mask": 1} if primary_thread_affinity else {}),
                                          **({"compiler_telemetry_path": telemetry["path"]} if compiler else {}))
            receipt = {key: value for key, value in result.items() if key != "counters"}
            receipt.update(command_kind="compile" if compiler else "runtime",
                           compiler_telemetry_sha256=None if telemetry is None else telemetry["sha256"])
            if cold:
                receipt["build_dir"] = env["CARGO_BUILD_BUILD_DIR"]
            if executed[1:3] == ["--bench", "--exact"] or (primary_thread_affinity and divan_timing):
                receipt["divan_arguments"] = executed[1:]
            if result["returncode"]:
                raise subprocess.CalledProcessError(result["returncode"], executed)
            if (result.get("descendants_exited") is not True
                    or result.get("job_memory_limit_bytes") != memory_limit):
                raise ValueError("owned process tree or aggregate memory limit was not verified")
            if compiler:
                if (check_windows_compiler_telemetry(self.protocol) != telemetry
                        or receipt.get("compiler_telemetry_path") != self.compiler_telemetry):
                    raise ValueError("compiler telemetry image changed during the build")
            resources = {"real_seconds": result["leader_elapsed_seconds"] if compiler else result["elapsed_seconds"],
                         "direct_process": result["counters"], "process_receipt": receipt}
            evidence.validate_windows_resource_counters(resources, expected_kind="compile" if compiler else "runtime",
                                                       primary_thread_affinity_required=primary_thread_affinity,
                                                       rank_cold=cold)
            if timed:
                write_json(destination.with_suffix(".resources.json"), resources)
        except BaseException as error:
            failure = {"error": str(error)}
            if hasattr(error, "windows_process_receipt"):
                failure["windows_process_receipt"] = error.windows_process_receipt
            raise
        finally:
            self.event("processes.jsonl", {
                "phase": "finish", "destination": str(destination),
                "returncode": None if result is None else result["returncode"], **failure,
                "job_memory_limit_bytes": None if result is None else result.get("job_memory_limit_bytes"),
                "process_receipt": receipt,
            })


def execute_pairs(protocol, roots, executables, targets, paired, cargo, env, monitor, *,
                  blocks=None, workloads=None, experimental_rank=False):
    if experimental_rank and "CARGO_BUILD_BUILD_DIR" in env:
        raise ValueError("ambient Cargo build directory is not permitted in rank capture")
    schedule = enumerate(evidence.seeded_order_schedule(protocol), 1) if blocks is None else blocks
    for block, order in schedule:
        monitor.idle(f"block-{block:02d}", sustained=True)
        for scenario in protocol["runtime"]["gated_scenarios"]:
            if workloads is not None and f"runtime/{scenario}" not in workloads:
                continue
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
                        *(evidence.isolated_divan_arguments(protocol, scenario, experimental_rank=experimental_rank)
                          if evidence.is_windows_protocol(protocol) else []),
                    ],
                    destination,
                    roots[role],
                    env,
                )
                evidence.validate_isolated_transcript(destination, scenario)
        for fixture in protocol["compile"]["fixtures"]:
            if workloads is not None and f"compile/{fixture}" not in workloads:
                continue
            for role in order.split("-"):
                target = targets / f"compile-{fixture}-{block:02d}-{role}"
                if target.exists() or (experimental_rank and target.is_symlink()):
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
                    {**env, "CARGO_TARGET_DIR": str(target), "CARGO_INCREMENTAL": "0",
                     **({"CARGO_BUILD_BUILD_DIR": str(target)} if experimental_rank else {})},
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
    sealed = {mode: [] for mode in captures}
    with (directory / "blocks.jsonl").open("x", encoding="utf-8") as transcript:
        for block, order in enumerate(evidence.seeded_order_schedule(protocol), 1):
            for mode in evidence.interleaved_mode_order(protocol, block):
                row = {"block": block, "mode": mode, "capture_sha256": capture_hash}
                for phase in ("start", "finish"):
                    measured = {"sha256": paired_block_hashes(directory, protocol, block, mode)} if phase == "finish" else {}
                    transcript.write(json.dumps({**row, **measured, "phase": phase, "recorded_at": now(),
                                                 "monotonic": time.monotonic()}, allow_nan=False) + "\n")
                    transcript.flush()
                    if phase == "finish" and protocol["method_version"] == evidence.WINDOWS_V6_METHOD:
                        sealed[mode].append({"block": block, "sha256": measured["sha256"]})
                        try:
                            if [item["block"] for item in sealed[mode]] != list(range(1, block + 1)):
                                raise ValueError("sealed paired prefix is not contiguous")
                            for item in sealed[mode]:
                                if paired_block_hashes(directory, protocol, item["block"], mode) != item["sha256"]:
                                    raise ValueError("sealed paired prefix hash changed")
                            paired = captures[mode]["paired"]
                            failures = evidence.paired_prefix_rejections(
                                BUDGET, protocol, paired / "subject", paired / "control", block)
                        except BaseException:
                            for capture in captures.values():
                                retain_runtime_products(capture)
                            raise
                        if failures:
                            retained = {name: retain_runtime_products(capture) for name, capture in captures.items()}
                            write_json(directory / "prefix-rejection.json", {
                                "complete": False, "passed": False, "qualification_acceptance_checked": False,
                                "status": "irrecoverable-prefix-rejection", "mode": mode, "block": block,
                                "protocol_sha256": evidence._protocol_sha256(protocol), "capture_sha256": capture_hash,
                                "blocks_sha256": evidence._sha256(directory / "blocks.jsonl"),
                                "failed_gates": failures, "sealed_mode_prefix": sealed[mode],
                                "retained_runtime_products": retained,
                            })
                            raise ValueError("paired capture has an irrecoverable final upper-bound failure")
                    if phase == "start":
                        capture = captures[mode]
                        execute_pairs(protocol, capture["roots"], capture["executables"],
                                      capture["targets"], capture["paired"], cargo, env, monitor,
                                      blocks=[(block, order)])


def paired_block_hashes(directory, protocol, block, mode, workloads=None):
    """Seal each completed transcript and its counters before the next block starts."""
    paths = []
    for role in ("subject", "control"):
        paired = Path(mode) / "paired" / role
        for family, names in (("runtime", protocol["runtime"]["gated_scenarios"]),
                              ("compile", protocol["compile"]["fixtures"])):
            for name in names:
                if workloads is not None and f"{family}/{name}" not in workloads:
                    continue
                transcript = paired / family / name.replace("/", "__") / f"block-{block:02d}.txt"
                paths.extend((transcript, transcript.with_suffix(".resources.json")))
                if evidence.is_current_mac_protocol(protocol):
                    paths.extend((transcript.with_suffix(".memory.json"), transcript.with_suffix(".exit.json")))
    for path in paths:
        if (directory / path).is_symlink():
            raise ValueError("interleaved measurement became a symlink")
    return {path.as_posix(): evidence._sha256(directory / path) for path in paths}


def collect_rank_timing(protocol, captures, cargo, env, monitor, directory, decide, *, single_look=False, two_look=False):
    """Retire a workload only after its paired and independent control gates decide it."""
    if set(captures) != {"candidate-reference", "reference-reference-aa"}:
        raise ValueError("rank capture requires both independent modes")
    counts = {f"{family}/{name}": 0 for family, names in (
        ("runtime", protocol["runtime"]["gated_scenarios"]), ("compile", protocol["compile"]["fixtures"]),
    ) for name in names}
    pending = set(counts)
    seals = {mode: {} for mode in captures}
    capture_hash = evidence._sha256(directory / "capture.json")
    decision = "continue"
    method = rank_method_identity(**look_options(single_look, two_look))
    if json.loads((directory / "capture.json").read_text(encoding="utf-8")).get("experiment") != method:
        raise ValueError("missing or changed prospective method identity before measurement")
    for scenario in protocol["runtime"]["gated_scenarios"]:
        seconds = method["runtime_window_seconds"].get(scenario)
        if evidence.isolated_divan_arguments(protocol, scenario, experimental_rank=True) != [
            "--min-time", seconds, "--max-time", seconds,
        ]:
            raise ValueError("runtime window differs from frozen v6 before measurement")
    with (directory / "experimental-blocks.jsonl").open("x", encoding="utf-8") as events:
        schedule = evidence.timing_schedule(protocol, two_look=two_look)
        for block, order in enumerate(schedule, 1):
            for mode in evidence.interleaved_mode_order(protocol, block, two_look=two_look):
                capture = captures[mode]
                event = {"block": block, "mode": mode, "workloads": sorted(pending), "capture_sha256": capture_hash}
                events.write(json.dumps({**event, "phase": "start", "monotonic": time.monotonic()}) + "\n")
                events.flush()
                execute_pairs(protocol, capture["roots"], capture["executables"], capture["targets"], capture["paired"],
                              cargo, env, monitor, blocks=[(block, order)], workloads=pending, experimental_rank=True)
                hashes = paired_block_hashes(directory, protocol, block, mode, pending)
                events.write(json.dumps({**event, "phase": "finish", "monotonic": time.monotonic(), "sha256": hashes}) + "\n")
                events.flush()
                seals[mode].update(hashes)
            for key in pending:
                counts[key] += 1
            gates = decide(directory, protocol, BUDGET, counts, seals)
            if set(gates) != set(counts) or any(row.get("blocks") != counts[key]
                    or row.get("decision") not in (("continue", "pass", "fail", "inconclusive")
                        if single_look or two_look else ("continue", "pass", "fail")) for key, row in gates.items()):
                raise ValueError("missing or invalid workload decision")
            if any(row["decision"] == "fail" for row in gates.values()):
                decision = "fail"
                break
            if (single_look or two_look) and any(row["decision"] == "inconclusive" for row in gates.values()):
                decision = "inconclusive"
                break
            outstanding = {key for key in counts if gates[key]["decision"] == "continue"}
            if not outstanding <= pending:
                raise ValueError("a retired timing workload no longer passes")
            pending = outstanding
            if not pending:
                decision = "pass"
                break
    if decision == "continue":
        raise ValueError("experimental timing did not decide all gates at the cap")
    return {"decision": decision, "counts": counts, "seals": seals, "gates": gates,
            "events_sha256": evidence._sha256(directory / "experimental-blocks.jsonl"), "capture_sha256": capture_hash}


def verify_rank_schedule(directory, protocol, timing, *, two_look=False):
    """Replay the actual ledger, including the independent control and every retained prefix."""
    log = directory / "experimental-blocks.jsonl"
    capture_hash = evidence._sha256(directory / "capture.json")
    if capture_hash != timing["capture_sha256"] or evidence._sha256(log) != timing["events_sha256"]:
        raise ValueError("experimental capture or block log changed")
    counts = timing["counts"]
    keys = {f"runtime/{k}" for k in protocol["runtime"]["gated_scenarios"]} | {
        f"compile/{k}" for k in protocol["compile"]["fixtures"]}
    cap = len(evidence.timing_schedule(protocol, two_look=two_look))
    if keys != set(counts) or any(type(n) is not int or not 1 <= n <= cap for n in counts.values()):
        raise ValueError("experimental timing has missing or invalid workload counts")
    events = [json.loads(s) for s in log.read_text(encoding="utf-8").splitlines()]
    if len(events) != 4 * max(counts.values()):
        raise ValueError("incomplete experimental block log")
    sealed = {mode: {} for mode in ("candidate-reference", "reference-reference-aa")}
    previous = -math.inf
    index = 0
    for block in range(1, max(counts.values()) + 1):
        active = sorted(k for k in keys if counts[k] >= block)
        for mode in evidence.interleaved_mode_order(protocol, block, two_look=two_look):
            for phase in ("start", "finish"):
                event = events[index]
                index += 1
                clock = event["monotonic"]
                if (event.get("block") != block or event.get("mode") != mode or event.get("phase") != phase
                        or event.get("capture_sha256") != capture_hash or event.get("workloads") != active
                        or not math.isfinite(clock) or clock < previous):
                    raise ValueError("experimental collection violated the paired block schedule")
                previous = clock
                if phase == "finish":
                    observed = paired_block_hashes(directory, protocol, block, mode, active)
                    if observed != event.get("sha256"):
                        raise ValueError("experimental block inputs changed")
                    sealed[mode].update(observed)
    if sealed != timing["seals"]:
        raise ValueError("experimental evidence differs from its collected blocks")


def retain_runtime_products(capture):
    """Preserve exact failed products before their temporary build trees unwind."""
    retained = capture["staging"] / "builds/failed-runtime-products"
    retained.mkdir()
    hashes = {}
    for role, executable in capture["executables"].items():
        destination = retained / (role + executable.suffix)
        shutil.copy2(executable, destination)
        digest = evidence._sha256(destination)
        if digest != capture["metadata"][f"{role}_executable_sha256"]:
            raise ValueError(f"retained runtime executable hash mismatch: {role}")
        hashes[role] = digest
    return hashes


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
    for line in log.read_text(encoding="utf-8").splitlines():
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


def registry_benchmark_preparation(donor, cargo_home, backend):
    """Seed only ordinary benchmark builds, from the first, cold reference arm."""
    prepared = set()
    seed = None

    def build(root, target, role, staging, tools, env, monitor, *, lock_sha256):
        nonlocal seed
        mode = staging.name
        if (mode not in ("reference-reference-aa", "candidate-reference")
                or role not in ("control", "subject")
                or target != donor.parent.parent / mode / role
                or root != target.parent / f"source-{role}"
                or target.resolve() != target or target.exists() or target.is_symlink()
                or target in prepared or (not prepared and target != donor)):
            raise ValueError("registry reuse requires a new ordinary benchmark target after the cold donor")
        log = staging / "builds" / f"{role}.txt"
        allowed = backend.seed_benchmark(seed, target, lock_sha256=lock_sha256) if seed is not None else {}
        executable = benchmark(root, target, role, staging, tools, env, monitor)
        if target == donor:
            # The donor check rejects any reused unit before preparing a seed.
            seed = backend.prepare_seed(target, log, cargo_home, lock_sha256=lock_sha256)
        else:
            backend.verify_rebuild(log, allowed)
        prepared.add(target)
        return executable

    return build


def prepare_pair_capture(protocol, budget, mode, staging, targets, reference, controller, control,
                         host, tools, env, monitor, hashes, registry_build=None, *, measured_subject=None,
                         two_look=False):
    """Prepare independently built arms without starting the measured blocks."""
    candidate = measured_subject if measured_subject is not None else controller
    reference_kind = native_release_reference.composition_reference()["mode"]
    subject = candidate if mode == "candidate-reference" else control
    candidate_root = ROOT if measured_subject is None else Path(candidate["root"])
    subject_root = candidate_root if mode == "candidate-reference" else reference
    revisions = ({"controller_revision": controller["revision"], "measured_subject_revision": candidate["revision"]}
                 if measured_subject is not None else {})
    staging.mkdir()
    targets.mkdir()
    write_json(staging / "inputs.json", {
        "mode": mode, "reference_kind": reference_kind, "recorded_at": now(), "controller": controller,
        "subject": subject, "control": control, "host": host, "input_hashes": hashes,
        **({"measured_subject": measured_subject} if measured_subject is not None else {}),
        "tools": {name: {"path": path, "sha256": evidence._sha256(Path(path))}
                  for name, path in tools.items()},
        **({"memory_limit_bytes": monitor.memory_limit_bytes}
           if evidence.is_current_mac_protocol(protocol) else {}),
    })
    with (staging / "protocol.json").open("xb") as copied_protocol:
        copied_protocol.write(protocol_path(protocol).read_bytes())
    write_json(staging / "plan.json", evidence.measurement_plan(protocol, mode, two_look=two_look))
    identities = {"subject": subject, "control": control}
    original_roots = {"subject": subject_root, "control": reference}
    roots = {role: targets / f"source-{role}" for role in original_roots}
    sources = {
        role: stage_source(original_roots[role], identities[role], roots[role], protocol)
        for role in roots
    }
    (staging / "sources").mkdir()
    for role, receipt in sources.items():
        receipt.update(revisions)
        write_json(staging / "sources" / f"{role}.json", receipt)
    executables = {
        role: (registry_build or benchmark)(
            roots[role], targets / role, role, staging, tools, env, monitor,
            **({"lock_sha256": identities[role]["hashes"]["Cargo.lock"]} if registry_build else {}),
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
        "reference_kind": reference_kind,
        **({"compiler_telemetry_path": tools["compiler_telemetry"]} if evidence.is_windows_protocol(protocol) else {}),
        "method_version": protocol["method_version"],
        "protocol_sha256": evidence._protocol_sha256(protocol),
        "budget_sha256": evidence.BUDGET_SHA256,
        "allocation_manifest_sha256": evidence.ALLOCATION_MANIFEST_SHA256,
        "seed": protocol["randomisation"]["seed"],
        "runner": protocol["runner"],
        "toolchain": evidence.expected_toolchain(protocol),
        "os_version": host["identity"]["os_version"],
        "os_build": host["identity"]["os_build"],
        "plan_sha256": evidence._sha256(staging / "plan.json"),
        "recorded_at": now(),
        "subject_role": roles["subject"],
        "control_role": roles["control"],
        "subject_revision": subject["revision"],
        "control_revision": control["revision"],
        **revisions,
        "subject_lock_sha256": subject["hashes"]["Cargo.lock"],
        "control_lock_sha256": control["hashes"]["Cargo.lock"],
        "qualification_script_sha256": evidence._sha256(Path(__file__)),
        **({"memory_watchdog_sha256": evidence._sha256(ROOT / "scripts/guard_cargo_memory.py"),
            "memory_limit_bytes": monitor.memory_limit_bytes}
           if evidence.is_current_mac_protocol(protocol) else {}),
        "evaluator_script_sha256": evidence._sha256(
            ROOT / "scripts/composition_performance.py"
        ),
        "capture_script_sha256": controller["hashes"][
            "scripts/capture-composition-performance.sh"
        ],
        "workload_sha256": sources["subject"]["effective"]["crates/rca-core/benches/composition.rs"],
        "zero_host_sha256": sources["subject"]["effective"]["crates/rca-core/tests/composition_qualification/main.rs"],
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
    if evidence.is_windows_protocol(protocol):
        metadata.update(platform="windows", target="x86_64-pc-windows-msvc")
    return {"mode": mode, "staging": staging, "targets": targets, "subject": subject,
            "roots": roots, "sources": sources, "executables": executables,
            "roles": roles, "metadata": metadata, "paired": staging / "paired"}


def capture_diagnostics(capture, protocol, env, monitor):
    """Run the full diagnostic list independently of the timing stopping rule."""
    staging, roots = capture["staging"], capture["roots"]
    executables, roles, metadata = capture["executables"], capture["roles"], capture["metadata"]
    for role, executable in executables.items():
        monitor.run(
            [str(executable), "--bench"],
            staging / "diagnostics" / f"{roles[role]}.txt",
            roots[role],
            env,
        )
    evidence.validate_protocol_diagnostics(
        staging / "diagnostics", tuple(roles.values()), protocol,
        metadata.get("memory_watchdog_sha256"),
        metadata.get("memory_limit_bytes"),
    )


def finish_pair_capture(capture, protocol, controller, env, monitor, *,
                        windows_complete=None, tools=None, baseline_path=None):
    """Evaluate all paired and absolute gates, preserving failed measurements."""
    mode, staging, targets = capture["mode"], capture["staging"], capture["targets"]
    subject, roots, roles = capture["subject"], capture["roots"], capture["roles"]
    capture_diagnostics(capture, protocol, env, monitor)
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
    if evidence.is_windows_protocol(protocol):
        if windows_complete is not None:
            finish_windows_complete(capture, protocol, env, monitor, tools, windows_complete,
                                    subject_report, reports, baseline_path)
        return complete_pair_capture(capture, reports, windows=True)
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
    reports["absolute.json"] = absolute
    return complete_pair_capture(capture, reports)


def finish_windows_complete(capture, protocol, env, monitor, tools, policy,
                            timing_report, reports, baseline_path):
    """Seal reference-only ceilings before collecting candidate absolute evidence."""
    import composition_windows_absolute as absolute_capture
    import composition_windows_complete as complete

    reference = capture["mode"] == "reference-reference-aa"
    if reference:
        if baseline_path.exists() or baseline_path.is_symlink():
            raise FileExistsError(baseline_path)
    else:
        if (baseline_path.is_symlink() or not baseline_path.is_file()
                or evidence._sha256(baseline_path) != capture["windows_baseline_sha256"]):
            raise ValueError("Windows reference baseline changed")
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
        if protocol.get("method_version") == evidence.WINDOWS_V6_METHOD:
            failed = complete.timing_failures(policy, BUDGET, timing_report, baseline)
            if failed:
                staging = capture["staging"]
                for name, report in reports.items():
                    write_json(staging / name, report_paths(report, staging))
                write_json(staging / "absolute-timing-rejection.json", {
                    "complete": False, "passed": False, "failed_gates": failed,
                    "windows_reference_baseline_sha256": capture["windows_baseline_sha256"],
                    "retained_runtime_products": retain_runtime_products(capture),
                })
                raise ValueError("candidate failed an absolute timing gate before allocation and binary capture")
    raw = absolute_capture.capture_windows_absolute(capture, protocol, env, monitor, tools)
    if reference:
        baseline = complete.seal_reference(policy, BUDGET, timing_report, raw)
        # Do not rewrite paths inside the sealed reference observations.
        write_json(baseline_path, baseline)
        capture["windows_baseline_sha256"] = evidence._sha256(baseline_path)
    reports["absolute-windows.json"] = complete.evaluate(
        policy, BUDGET, timing_report, raw, baseline)
    if evidence._sha256(baseline_path) != capture["windows_baseline_sha256"]:
        raise ValueError("Windows reference baseline changed")


def verify_pair_products(capture):
    """Check that both measured binaries and source receipts still match the staged inputs."""
    staging, metadata = capture["staging"], capture["metadata"]
    for role, executable in capture["executables"].items():
        if evidence._sha256(executable) != metadata[f"{role}_executable_sha256"]:
            raise ValueError("benchmark executable changed during capture")
    for role in capture["roots"]:
        if evidence._sha256(staging / "sources" / f"{role}.json") != metadata[
            "workload_provenance"][role]["receipt_sha256"]:
            raise ValueError("staged composition source receipt changed")
        validate_staged_source(staging / "sources" / f"{role}.json")


def look_options(single_look, two_look):
    """Select one experimental confidence rule without changing the original route."""
    if single_look and two_look:
        raise ValueError("select only one experimental look policy")
    return {"single_look": True} if single_look else {"two_look": True} if two_look else {}


def rank_receipt(*, single_look=False, two_look=False):
    """Give each experimental result a distinct filename."""
    return ("experimental-two-look-result.json" if two_look else
            "experimental-single-look-result.json" if single_look else "experimental-rank-result.json")


def rank_method_identity(*, single_look=False, two_look=False):
    """Pin the collection and replay code before the first experimental measurement."""
    names = ("composition_runner.py", "composition_performance.py", "release_baseline.py",
             "native_release_reference.py", "composition_cold_timing.py",
             "composition_timing_inputs.py", "composition_product_commands.py",
             "composition_windows_complete.py", "composition_windows_absolute.py",
             "composition_registry_seed.py", "windows_strip.py", "windows_process.py", "capture-windows-performance.py")
    protocol = evidence.load_protocol(WINDOWS_PROTOCOL)
    if evidence._protocol_sha256(protocol) != evidence.WINDOWS_V6_PROTOCOL_SHA256:
        raise ValueError("experimental rank windows require the frozen Windows v6 protocol")
    windows = {s: evidence.isolated_divan_arguments(protocol, s)[1] for s in protocol["runtime"]["gated_scenarios"]}
    method = {"method": "finite-rank-v3", "runtime_window_seconds": windows,
              "scripts": {n: evidence._sha256(ROOT / "scripts" / n) for n in names}}
    look_options(single_look, two_look)
    if two_look:
        method.update(method=evidence.TWO_LOOK_METHOD, relative_confidence={
            "looks": [{"observations_per_order": 7, "upper_rank": 7}, {"observations_per_order": 12, "upper_rank": 11}],
            "combination": "minimum-simultaneous-upper-bound-per-order-then-geometric-mean",
            "stopping": "first-decision-over-all-completions",
        }, absolute_reference_blocks=20, maximum_blocks=24,
            order_schedule=evidence.timing_schedule(protocol, two_look=True))
    elif single_look:
        method.update(method="single-look-first-seven-v1",
                      relative_confidence={"observations_per_order": 7, "upper_rank": 7, "looks": 1},
                      absolute_reference_blocks=20)
    return method


def verify_rank_git_sources(directory, manifest, expected_controller, expected_subject):
    """Anchor archived product bytes and controller scripts to the externally requested commits."""
    revisions = rank_revisions(manifest.get("controller"), manifest.get("measured_subject"),
                               expected_controller, expected_subject)
    selection = native_release_reference.composition_reference()
    reference = selection["revision"]
    if (manifest.get("reference_kind") != selection["mode"]
            or (expected_subject == reference) != (selection["mode"] == "first-release-enrolment")):
        raise ValueError("rank sources do not match the selected release reference")
    archives = {}
    for rev in {expected_controller, expected_subject, reference}:
        archive = subprocess.check_output(["git", "--no-replace-objects", "archive", "--format=tar", rev], cwd=ROOT)
        with tarfile.open(fileobj=io.BytesIO(archive)) as stream:
            hashes = {p.name: hashlib.sha256(stream.extractfile(p).read()).hexdigest()
                      for p in stream if p.isfile()}
        archives[rev] = (read(["git", "--no-replace-objects", "rev-parse", f"{rev}^{{tree}}"], cwd=ROOT), hashes)
    for name in ("controller", "measured_subject"):
        identity = manifest[name]
        tree, original = archives[identity["revision"]]
        if identity.get("tree") != tree or not isinstance(identity.get("hashes"), dict) or any(
            original.get(key) != digest for key, digest in identity["hashes"].items()
        ):
            raise ValueError(f"rank {name} tree or source hashes differ from expected Git commit")
    if any(archives[expected_controller][1].get(f"scripts/{name}") != digest
           for name, digest in manifest["experiment"]["scripts"].items()):
        raise ValueError("rank method scripts differ from expected controller commit")
    for mode in ("reference-reference-aa", "candidate-reference"):
        for role in ("subject", "control"):
            rev = expected_subject if (mode, role) == ("candidate-reference", "subject") else reference
            source = json.loads((directory / mode / "sources" / f"{role}.json").read_bytes())
            if (source.get("revision") != rev or (source.get("tree"), source.get("original")) != archives[rev]
                    or any(source.get(k) != v for k, v in revisions.items())):
                raise ValueError(f"rank {mode} {role} source differs from expected Git archive")
    return revisions


def experimental_composition_result(directory, timing_digest, products, diagnostics, *,
                                    expected_controller=None, expected_subject=None, single_look=False, two_look=False):
    """Recompute a prospective decision from raw evidence; never approve a release."""
    import composition_cold_timing as bounds
    import composition_windows_complete as complete

    protocol = evidence.load_protocol(WINDOWS_PROTOCOL)
    policy = complete.load_policy(WINDOWS_COMPLETE_POLICY)
    manifest = json.loads((directory / "capture.json").read_text(encoding="utf-8"))
    options = look_options(single_look, two_look)
    if manifest.get("experiment") != rank_method_identity(**options):
        raise ValueError("missing or changed prospective method identity")
    revisions = verify_rank_git_sources(directory, manifest, expected_controller, expected_subject)
    after_path = directory / "source-identities-after.json"
    if after_path.resolve() != after_path.absolute() or not after_path.is_file():
        raise ValueError("missing or redirected final rank source identities")
    after = json.loads(after_path.read_bytes())
    if after != {"capture_sha256": evidence._sha256(directory / "capture.json"),
                 "controller": manifest["controller"], "measured_subject": manifest["measured_subject"],
                 "control": manifest["control"]}:
        raise ValueError("rank source identities changed after measurement")
    timing_path = directory / "experimental-timing.json"
    if timing_path.is_symlink() or evidence._sha256(timing_path) != timing_digest:
        raise ValueError("changed experimental timing result")
    timing = json.loads(timing_path.read_text(encoding="utf-8"))
    extended = {"two_look": True} if two_look else {}
    verify_rank_schedule(directory, protocol, timing, **extended)
    gates = bounds.timing_decisions(directory, protocol, BUDGET, timing["counts"], timing["seals"],
                                    **options)
    decision = bounds._decision([g["decision"] for g in gates.values()]) if gates else "continue"
    if decision == "continue" or decision != timing["decision"] or gates != timing["gates"]:
        raise ValueError("experimental timing decision does not follow from sealed measurements")
    product_report = None
    if decision == "pass":
        from composition_timing_inputs import verify_terminal_processes
        verify_terminal_processes(directory, protocol, **extended)
        for mode, roles in (("reference-reference-aa", ("reference-b", "reference-a")),
                            ("candidate-reference", ("candidate", "reference"))):
            location = directory / mode / "diagnostics"
            evidence.validate_protocol_diagnostics(location, roles, protocol)
            actual = {p.name: evidence._sha256(p) for p in location.iterdir() if p.is_file() and not p.is_symlink()}
            if actual != diagnostics[mode]:
                raise ValueError("experimental diagnostics changed")
        product_report = complete.load_experimental_products(directory, policy, BUDGET, products)
        if not product_report["passed"]:
            decision = "fail"
    elif diagnostics or products:
        raise ValueError("unexpected products after a decisive timing rejection")
    return {"schema": ("experimental-windows-composition-two-look-v2" if two_look else
                       "experimental-windows-composition-first-seven-v1" if single_look
                       else "experimental-windows-composition-finite-rank-v3"), "decision": decision,
            **revisions, "reference_revision": evidence.expected_reference_revision(protocol),
            "reference_kind": manifest["reference_kind"],
            "prior_release_comparison": manifest["reference_kind"] == "release-comparison",
            "source_identities_after_sha256": evidence._sha256(after_path),
            "scope": "windows-composition-only", "release_authorised": False,
            "timing_sha256": timing_digest, "capture_sha256": timing["capture_sha256"],
            "diagnostics": diagnostics, "raw_product_sha256": products, "product_gates": product_report}


def verify_experimental_composition(directory, *, expected_controller=None, expected_subject=None, single_look=False,
                                    two_look=False):
    """Replay raw measurements without writing a new completion receipt."""
    options = look_options(single_look, two_look)
    name = rank_receipt(**options)
    path = directory / name
    others = {rank_receipt(), rank_receipt(single_look=True), rank_receipt(two_look=True), "complete.json"} - {name}
    if path.is_symlink() or any((directory / n).exists() or (directory / n).is_symlink() for n in others):
        raise ValueError("mixed or redirected experimental completion receipt")
    claimed = json.loads(path.read_text(encoding="utf-8"))
    actual = experimental_composition_result(directory, claimed["timing_sha256"],
                                              claimed["raw_product_sha256"], claimed["diagnostics"],
                                              expected_controller=expected_controller, expected_subject=expected_subject,
                                              **options)
    if actual != claimed:
        raise ValueError("experimental decision differs from independent replay")
    return actual


def replay_published_composition(directory, tag, expected_controller, expected_subject, **look):
    """Check a past release against its own predecessor without qualifying a new candidate."""
    if not all(isinstance(value, str) and evidence._REVISION.fullmatch(value)
               for value in (expected_controller, expected_subject)):
        raise ValueError("historical replay needs exact controller and subject commits")
    if (read(["git", "--no-replace-objects", "rev-parse", "HEAD"], cwd=ROOT) != expected_controller
            or read(["git", "--no-replace-objects", "status", "--porcelain=v1", "--untracked-files=all"], cwd=ROOT)):
        raise ValueError("historical replay needs the clean original controller checkout")
    with native_release_reference.historical_composition_reference(tag, expected_subject) as identity:
        decision = verify_experimental_composition(directory, expected_controller=expected_controller,
                                                   expected_subject=expected_subject, **look)
        return {"purpose": "historical-replay", "subject_tag": tag, "reference_tag": identity["reference_tag"],
                "reference_revision": identity["revision"], "reference_kind": identity["mode"],
                "controller_revision": expected_controller, "measured_subject_revision": expected_subject,
                "historical_decision": decision["decision"], "current_qualification": False, "release_authorised": False}


def collect_experimental_composition(protocol, captures, env, tools, monitor, staging, *, single_look=False, two_look=False):
    """Capture real product and diagnostic evidence whenever the complete timing set passes."""
    import composition_cold_timing as bounds
    import composition_windows_absolute as absolute

    from functools import partial
    options = look_options(single_look, two_look)
    score = partial(bounds.timing_decisions, **options) if options else bounds.timing_decisions
    timing = collect_rank_timing(protocol, captures, tools["cargo"], env, monitor, staging, score, **options)
    timing_path = staging / "experimental-timing.json"
    write_json(timing_path, timing)
    digest = evidence._sha256(timing_path)
    products, diagnostics = {}, {}
    for mode in ("reference-reference-aa", "candidate-reference"):
        capture = captures[mode]
        if timing["decision"] == "pass":
            capture_diagnostics(capture, protocol, env, monitor)
            logs = capture["staging"] / "diagnostics"
            diagnostics[mode] = {p.name: evidence._sha256(p) for p in logs.iterdir() if p.is_file() and not p.is_symlink()}
            absolute.capture_windows_absolute(capture, protocol, env, monitor, tools)
            products[mode] = evidence._sha256(capture["staging"] / "absolute-windows-raw/observation.json")
        verify_pair_products(capture)
    return digest, products, diagnostics


def complete_pair_capture(capture, reports, *, windows=False):
    """Seal reports only after the measured source and binaries remain unchanged."""
    verify_pair_products(capture)
    staging = capture["staging"]
    for name, report in reports.items():
        write_json(staging / name, report_paths(report, staging))
    passed = all(report.get("passed", True) for report in reports.values())
    scope = {"qualification_scope": "windows-paired-only", "mac_absolute_qualified": False} if windows else {}
    if windows and "windows_baseline_sha256" in capture:
        scope.update(qualification_scope="windows-complete",
                     windows_reference_baseline_sha256=capture["windows_baseline_sha256"],
                     absolute_windows_sha256=evidence._sha256(staging / "absolute-windows.json"))
    write_json(staging / "complete.json", {"completed_at": now(), "passed": passed, **scope})
    return passed


def validate_interleaved_capture(directory, protocol):
    """Reject detached controls, changed plans, missing blocks and reordered evidence."""
    manifest = json.loads((directory / "capture.json").read_text(encoding="utf-8"))
    digest = evidence._sha256(directory / "capture.json")
    modes = ("candidate-reference", "reference-reference-aa")
    schedule = [{"block": block, "mode": mode}
                for block in range(1, len(evidence.seeded_order_schedule(protocol)) + 1)
                for mode in evidence.interleaved_mode_order(protocol, block)]
    if (not isinstance(manifest, dict) or manifest.get("protocol_sha256") != evidence._protocol_sha256(protocol)
            or manifest.get("reference_kind") != native_release_reference.composition_reference()["mode"]
            or manifest.get("schedule") != schedule or not isinstance(manifest.get("modes"), dict)
            or set(manifest["modes"]) != set(modes)):
        raise ValueError("interleaved capture plan differs")
    if evidence.is_current_mac_protocol(protocol):
        evidence.validate_mac_memory_limit(manifest.get("memory_limit_bytes"))
    for mode in modes:
        arm = directory / mode
        if manifest["modes"][mode] != {name: evidence._sha256(arm / name)
                                     for name in ("inputs.json", "plan.json")}:
            raise ValueError("interleaved capture inputs changed")
        inputs = json.loads((arm / "inputs.json").read_text(encoding="utf-8"))
        if (not isinstance(inputs, dict) or inputs.get("mode") != mode
                or inputs.get("reference_kind") != manifest["reference_kind"]
                or inputs.get("controller") != manifest.get("controller")
                or inputs.get("host") != manifest.get("host")
                or json.loads((arm / "plan.json").read_text(encoding="utf-8")) != evidence.measurement_plan(protocol, mode)):
            raise ValueError("interleaved capture identity differs")
        metadata = json.loads((arm / "metadata.json").read_text(encoding="utf-8"))
        if (not isinstance(metadata, dict) or metadata.get("capture_sha256") != digest
                or metadata.get("reference_kind") != manifest["reference_kind"]):
            raise ValueError("interleaved capture binding differs")
        if evidence.is_current_mac_protocol(protocol) and any(
                type(record.get("memory_limit_bytes")) is not int
                or record["memory_limit_bytes"] != manifest["memory_limit_bytes"]
                for record in (inputs, metadata)):
            raise ValueError("interleaved capture memory ceilings differ")
    rows = [json.loads(line) for line in (directory / "blocks.jsonl").read_text(encoding="utf-8").splitlines()]
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


def qualify(output, staging, reference, mode, aa_evidence=None, *, platform_name="macos",
            windows_complete=False, memory_limit_bytes=8 * 1024**3, experimental_rank=False,
            subject=None, expected_controller=None, expected_subject=None, single_look=False, two_look=False):
    native_release_reference.require_current_selection()
    if mode != "candidate-reference" or aa_evidence is not None:
        raise ValueError("composition qualification needs one contemporaneous two-mode capture")
    windows = platform_name == "windows"
    options = look_options(single_look, two_look)
    if options and not experimental_rank:
        raise ValueError("experimental look policies require experimental rank capture")
    if experimental_rank and not (windows and windows_complete):
        raise ValueError("experimental rank capture requires complete Windows qualification")
    if not experimental_rank and any(v is not None for v in (subject, expected_controller, expected_subject)):
        raise ValueError("independent product source applies only to experimental rank capture")
    if windows_complete and not windows:
        raise ValueError("--windows-complete requires --platform windows")
    if windows and sys.platform != "win32":
        raise ValueError("Windows paired qualification requires Windows")
    if not windows and (platform_name != "macos" or sys.platform != "darwin"):
        raise ValueError("canonical qualification requires arm64 macOS")
    selected_protocol = WINDOWS_PROTOCOL if windows else CURRENT_PROTOCOL
    protocol = evidence.load_protocol(selected_protocol)
    complete_policy = None
    if windows_complete:
        import composition_windows_complete as complete
        complete_policy = complete.load_policy(WINDOWS_COMPLETE_POLICY)
    env, tools = tool_environment(include_capture_tool=False) if windows else tool_environment()
    if windows:
        tools["python"] = str(Path(sys.executable).resolve(strict=True))
        if "host: x86_64-pc-windows-msvc" not in read([tools["rustc"], "-vV"]).splitlines():
            raise ValueError("Windows composition requires the pinned MSVC host toolchain")
        tools["compiler_telemetry"] = check_windows_compiler_telemetry(protocol)["path"]
        if windows_complete:
            tools["objcopy"] = windows_objcopy(tools)
            tools.update(windows_strip.preflight(env))
    host = windows_host() if windows else canonical_host()
    reference = Path(read(["git", "rev-parse", "--show-toplevel"], cwd=reference)).resolve()
    controller = source_identity(ROOT)
    control = source_identity(reference, reference=True)
    candidate = rank_subject(subject, controller, expected_controller, expected_subject) if experimental_rank else controller
    selection = native_release_reference.composition_reference()
    if (candidate["revision"] == control["revision"]) != (selection["mode"] == "first-release-enrolment"):
        raise ValueError("candidate and reference revisions do not match the selected release comparison")
    if experimental_rank and "CARGO_BUILD_BUILD_DIR" in env:
        raise ValueError("ambient Cargo build directory is not permitted in rank capture")
    capture_tool = None if windows else check_capture_tool(ROOT, tools["cargo"], env)
    budget = evidence.validate_budget_document(BUDGET)
    evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    pinned_files = [
        selected_protocol, BUDGET, evidence.DEFAULT_ALLOCATION_MANIFEST, Path(__file__),
        ROOT / "scripts/release_baseline.py", ROOT / "scripts/native_release_reference.py",
        ROOT / "scripts/composition_performance.py", ROOT / "scripts/qualify-composition-performance.sh",
        *(ROOT / item["source"] for item in protocol["common_workloads"]["files"].values()),
        *(Path(path) for path in tools.values()),
    ]
    # Windows 3.12.4+ creates the private scratch ACL for TemporaryDirectory.
    # Reuse covers third-party preparation only, never a measured cold build.
    reuse_registry = ((not windows and protocol["method_version"] == evidence.RUST100_METHOD)
                      or (windows and sys.version_info >= (3, 12, 4)
                          and protocol["method_version"] == evidence.WINDOWS_V6_METHOD))
    if reuse_registry:
        import composition_registry_seed as registry
        pinned_files.append(Path(registry.__file__).resolve())
    if experimental_rank:
        pinned_files.extend(ROOT / "scripts" / name for name in
                            ("composition_cold_timing.py", "composition_timing_inputs.py", "composition_product_commands.py"))
    if windows:
        pinned_files.extend(ROOT / "scripts" / name for name in
                            ("capture-windows-performance.py", "windows_process.py"))
        if windows_complete:
            pinned_files.extend((WINDOWS_COMPLETE_POLICY,
                                 ROOT / "scripts/composition_windows_complete.py",
                                 ROOT / "scripts/composition_windows_absolute.py",
                                 ROOT / "scripts/windows_strip.py"))
    else:
        pinned_files.extend((capture_tool, ROOT / "scripts/guard_cargo_memory.py"))
    hashes = {str(path): evidence._sha256(path) for path in pinned_files}
    staging.parent.mkdir(parents=True, exist_ok=True)
    staging.mkdir()
    monitor = (WindowsMonitor(staging / "telemetry", protocol, cargo=tools["cargo"],
                              compiler_telemetry=tools["compiler_telemetry"],
                              **({"experimental_rank": True} if experimental_rank else {}),
                              **({"complete": True} if windows_complete else {})) if windows
               else HostMonitor(staging / "telemetry", protocol, memory_limit_bytes=memory_limit_bytes))
    monitor.idle("initial", sustained=True)
    env.update(CARGO_PROFILE_BENCH_DEBUG="false", CARGO_PROFILE_BENCH_LTO="false",
               CARGO_PROFILE_BENCH_STRIP="true")
    modes = ("candidate-reference", "reference-reference-aa")
    baseline_path = staging / "Windows-reference-baseline.json"
    # Build trees are disposable. Transcripts and failed evidence remain in staging.
    with tempfile.TemporaryDirectory(prefix="rca-composition-paired.") as temporary:
        targets = Path(temporary).resolve()
        registry_build = None
        if reuse_registry:
            cargo_home = Path(env.get("CARGO_HOME") or Path.home() / ".cargo")
            if cargo_home.is_absolute():
                registry_build = registry_benchmark_preparation(
                    targets / "candidate-reference/control", cargo_home.resolve(), registry,
                )
        captures = {item: prepare_pair_capture(
            protocol, budget, item, staging / item, targets / item, reference,
            controller, control, host, tools, env, monitor, hashes, registry_build,
            **({"measured_subject": candidate} if experimental_rank else {}),
            **({"two_look": True} if two_look else {})) for item in modes}
        write_json(staging / "capture.json", {
            "protocol_sha256": evidence._protocol_sha256(protocol), "controller": controller, "host": host,
            "reference_kind": selection["mode"],
            **({"experiment": rank_method_identity(**options), "measured_subject": candidate, "control": control}
               if experimental_rank else {}),
            "modes": {item: {name: evidence._sha256(staging / item / name)
                             for name in ("inputs.json", "plan.json")} for item in modes},
            "schedule": [{"block": block, "mode": item}
                         for block in range(1, len(evidence.timing_schedule(protocol, two_look=two_look)) + 1)
                         for item in evidence.interleaved_mode_order(protocol, block, two_look=two_look)],
            **({"memory_limit_bytes": monitor.memory_limit_bytes} if not windows else {}),
        })
        capture_hash = evidence._sha256(staging / "capture.json")
        for capture in captures.values():
            capture["metadata"]["capture_sha256"] = capture_hash
            write_json(capture["staging"] / "metadata.json", capture["metadata"])
            evidence.validate_protocol_sources(capture["staging"], capture["metadata"], protocol)
        if experimental_rank:
            rank_evidence = collect_experimental_composition(protocol, captures, env, tools, monitor, staging,
                                                             **options)
            check_host_unchanged(host, monitor)
            after_controller = source_identity(ROOT)
            after_subject = rank_subject(subject, after_controller, controller["revision"], candidate["revision"])
            write_json(staging / "source-identities-after.json", {
                "capture_sha256": capture_hash, "controller": after_controller, "measured_subject": after_subject,
                "control": source_identity(reference, reference=True),
            })
            result = experimental_composition_result(staging, *rank_evidence, expected_controller=controller["revision"],
                                                     expected_subject=candidate["revision"], **options)
            if result["decision"] == "fail":
                for capture in captures.values():
                    retain_runtime_products(capture)
        else:
            execute_interleaved_pairs(protocol, captures, tools["cargo"], env, monitor, staging)
            passed = {}
            for item in (tuple(reversed(modes)) if windows_complete else modes):
                if windows_complete and item == "candidate-reference":
                    captures[item]["windows_baseline_sha256"] = captures[
                        "reference-reference-aa"]["windows_baseline_sha256"]
                options = ({"windows_complete": complete_policy, "tools": tools,
                            "baseline_path": baseline_path} if windows_complete else {})
                passed[item] = finish_pair_capture(captures[item], protocol, controller, env, monitor,
                                                    **options)
                if not passed[item]:
                    retain_runtime_products(captures[item])
                shutil.rmtree(captures[item]["targets"])
            if validate_interleaved_capture(staging, protocol) != capture_hash:
                raise ValueError("interleaved capture manifest changed")
    if not experimental_rank:
        check_host_unchanged(host, monitor)
        if source_identity(ROOT) != controller or source_identity(reference, reference=True) != control:
            raise ValueError("source or host identity changed during qualification")
    if any(evidence._sha256(Path(path)) != value for path, value in hashes.items()):
        raise ValueError("qualification input hash changed")
    if experimental_rank:
        if result["capture_sha256"] != capture_hash:
            raise ValueError("experimental capture manifest changed")
        write_json(staging / rank_receipt(**options), result)
        publish(staging, output)
        return 0 if result["decision"] == "pass" else 1
    if passed["reference-reference-aa"]:
        validate_aa_evidence(staging / "reference-reference-aa", protocol, controller, host,
                             capture=staging,
                             **({"windows_complete": complete_policy} if windows_complete else {}))
    complete_scope = {"qualification_scope": "windows-paired-only", "mac_absolute_qualified": False} if windows else {}
    if windows_complete:
        baseline_hash = evidence._sha256(baseline_path)
        if any(item["windows_baseline_sha256"] != baseline_hash for item in captures.values()):
            raise ValueError("Windows reference baseline changed")
        complete_scope.update(qualification_scope="windows-complete",
                              windows_reference_baseline_sha256=baseline_hash,
                              windows_complete_policy_sha256=hashes[str(WINDOWS_COMPLETE_POLICY)])
    write_json(staging / "complete.json", {
        "completed_at": now(), "passed": all(passed.values()),
        "reference_kind": selection["mode"], "prior_release_comparison": selection["mode"] == "release-comparison",
        "capture_sha256": capture_hash, "blocks_sha256": evidence._sha256(staging / "blocks.jsonl"),
        "modes": {item: {name: evidence._sha256(staging / item / name)
                         for name in receipt_files(protocol, windows_complete=windows_complete)} for item in modes},
        **complete_scope,
    })
    publish(staging, output)
    return 0 if all(passed.values()) else 1


def check_counter_configuration(root, cargo_home):
    """Reject Cargo overrides that can change a measured build."""
    configurations = [cargo_home / name for name in ("config", "config.toml")]
    for directory in (root.resolve(), *root.resolve().parents):
        configurations.extend(directory / ".cargo" / name for name in ("config", "config.toml"))
    if any(path.exists() or path.is_symlink() for path in configurations):
        raise ValueError("qualification rejects ambient Cargo configuration")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=("macos", "windows"), default="macos",
                        help="Windows captures paired gates only; Darwin absolute gates remain separate")
    parser.add_argument("--windows-complete", action="store_true",
                        help="add Windows allocation and reference-only timing/PE gates")
    parser.add_argument("--experimental-single-look", action="store_true",
                        help="use only the first seven pairs in each order for relative confidence; keep all absolute gates")
    parser.add_argument("--experimental-two-look", action="store_true",
                        help="extend an undecided first comparison to at most twelve pairs per order; keep all absolute gates")
    parser.add_argument("--experimental-rank", action="store_true",
                        help="exercise finite-rank Windows decisions without issuing a release receipt")
    parser.add_argument("--verify-experimental-rank", metavar="DIRECTORY",
                        help="replay an experimental decision from its retained raw observations")
    parser.add_argument("--replay-release", metavar="TAG",
                        help="verify a past published release only; requires its original controller and subject")
    parser.add_argument("--subject", type=Path, metavar="CHECKOUT",
                        help="measure this clean product checkout with the experimental Windows controller")
    parser.add_argument("--expected-controller", metavar="COMMIT", help="externally pinned full controller commit")
    parser.add_argument("--expected-subject", metavar="COMMIT", help="externally pinned full measured product commit")
    parser.add_argument("--memory-limit-gib", type=int, choices=range(1, 9),
                        help="Mac qualification ceiling in GiB, from 1 to 8; default: 8")
    parser.add_argument(
        "--mode",
        choices=(
            "candidate-reference",
            "reference-reference-aa",
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
        if args.replay_release is not None and not args.verify_experimental_rank:
            raise ValueError("--replay-release applies only to --verify-experimental-rank")
        look = look_options(args.experimental_single_look, args.experimental_two_look)
        if look and not (args.verify_experimental_rank or args.experimental_rank):
            raise ValueError("look policies require --experimental-rank or --verify-experimental-rank")
        if args.verify_experimental_rank:
            if (args.output or args.reference or args.windows_complete or args.experimental_rank or args.subject
                    or args.mode != "candidate-reference" or args.memory_limit_gib or args.aa_evidence
                    or any((args.check_prerequisites, args.check_staged_source, args.check_publication, args.check_lock))):
                raise ValueError("experimental readback cannot run a capture")
            if args.replay_release is not None:
                report = replay_published_composition(Path(args.verify_experimental_rank), args.replay_release,
                                                      args.expected_controller, args.expected_subject, **look)
                print(json.dumps(report))
                return 0
            decision = verify_experimental_composition(Path(args.verify_experimental_rank),
                                                      expected_controller=args.expected_controller,
                                                      expected_subject=args.expected_subject,
                                                      **look)
            print(json.dumps({key: decision[key] for key in ("scope", "decision", "controller_revision",
                                                              "measured_subject_revision", "release_authorised")}))
            return 0
        if not args.experimental_rank and any(v is not None for v in (args.subject, args.expected_controller, args.expected_subject)):
            raise ValueError("independent product source applies only to experimental rank capture")
        if args.experimental_rank and (args.subject or args.expected_subject or args.expected_controller):
            if not all(v and evidence._REVISION.fullmatch(v) for v in (args.expected_controller, args.expected_subject)):
                raise ValueError("rank source selection requires expected full controller and subject commits")
        if args.experimental_rank and (args.platform != "windows" or not args.windows_complete
                                      or args.mode != "candidate-reference" or args.aa_evidence
                                      or any((args.check_prerequisites, args.check_staged_source,
                                              args.check_publication, args.check_lock))):
            raise ValueError("experimental rank capture requires --platform windows --windows-complete")
        if args.memory_limit_gib is not None and (args.platform != "macos"
                or args.mode != "candidate-reference" or args.check_prerequisites
                or args.check_staged_source or args.check_publication or args.check_lock):
            raise ValueError("--memory-limit-gib applies only to canonical Mac qualification")
        if args.windows_complete and args.platform != "windows":
            raise ValueError("--windows-complete requires --platform windows")
        if args.platform == "windows" and args.mode != "candidate-reference":
            raise ValueError("Windows qualification supports only paired candidate/reference captures")
        if args.check_staged_source:
            print(json.dumps(validate_staged_source(args.check_staged_source)))
            return 0
        if args.check_prerequisites:
            if args.platform == "windows":
                if sys.platform != "win32":
                    raise ValueError("Windows paired qualification requires Windows")
                _env, tools = tool_environment(include_capture_tool=False)
                if "host: x86_64-pc-windows-msvc" not in read([tools["rustc"], "-vV"]).splitlines():
                    raise ValueError("Windows composition requires the pinned MSVC host toolchain")
                if args.windows_complete:
                    import composition_windows_complete as complete
                    complete.load_policy(WINDOWS_COMPLETE_POLICY)
                    windows_objcopy(tools)
                    windows_strip.preflight(_env)
                print("the pinned Windows Cargo, rustc and rustdoc are available")
            else:
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
        if not args.output or not args.reference:
            parser.error("output and reference checkout are required")
        if args.mode != "candidate-reference" or args.aa_evidence:
            raise ValueError("composition qualification captures both modes together; external A/A evidence is not accepted")
        output, staging = output_paths(args.output)
        selected_lock = Path(tempfile.gettempdir()) / LOCK.name if args.platform == "windows" else LOCK
        with machine_lock(selected_lock):
            return qualify(
                output, staging, Path(args.reference), args.mode, args.aa_evidence,
                **({"platform_name": "windows"} if args.platform == "windows" else {}),
                **({"windows_complete": True} if args.windows_complete else {}),
                **({"experimental_rank": True} if args.experimental_rank else {}),
                **look,
                **({"subject": args.subject, "expected_controller": args.expected_controller,
                    "expected_subject": args.expected_subject} if args.experimental_rank else {}),
                **({"memory_limit_bytes": (args.memory_limit_gib or 8) * 1024**3}
                   if args.platform == "macos" else {}),
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
