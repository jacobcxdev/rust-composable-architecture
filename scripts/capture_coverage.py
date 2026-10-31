#!/usr/bin/env python3
"""Retain selected native coverage evidence on an isolated Linux runner.

This captures native inputs, not a coverage percentage or production-source audit.
"""

import argparse
from dataclasses import dataclass
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time
import tomllib


TOOLCHAIN = "1.99.0"
RUSTFLAGS = "-C opt-level=s -C debuginfo=0 -C debug-assertions=yes -C overflow-checks=yes -C panic=unwind -C instrument-coverage -Z coverage-options=branch"


@dataclass(frozen=True)
class CaptureGroup:
    """Bind one Cargo package, feature set and list of ordinary libtest hosts."""

    package: str
    directory: str
    library: str
    features: frozenset[str]
    cargo_options: tuple[str, ...]
    tests: tuple[str, ...]
    include_library: bool = True


TARGETS = {
    "core-minimal": CaptureGroup(
        "rca-core", "rca-core", "rca_core", frozenset({"local"}),
        ("--no-default-features", "--features", "local"),
        ("store_actor", "local_store"),
    ),
    "core-rich": CaptureGroup(
        "rca-core", "rca-core", "rca_core",
        frozenset({"default", "diagnostics", "futures-stream", "local", "test-util"}),
        ("--features", "diagnostics,futures-stream,local,test-util"),
        ("structural_diagnostics", "mount_acquisition", "local_mount",
         "output_delivery", "local_output_delivery", "scoped_store",
         "keyed_operations", "shared_key_cache"),
    ),
    "core-removal": CaptureGroup(
        "rca-core", "rca-core", "rca_core",
        frozenset({"default", "diagnostics", "futures-stream", "local", "test-util"}),
        ("--features", "diagnostics,futures-stream,local,test-util"),
        ("removed_child", "local_removed_child"), include_library=False,
    ),
    "tokio": CaptureGroup(
        "rca-tokio", "rca-tokio", "rca_tokio", frozenset(), (),
        ("async_cleanup_host", "client_clock", "panic_subprocess",
         "synchronous_host", "thread_affine_host"),
    ),
    "listener-std": CaptureGroup(
        "rca-event-listener", "dependencies/rca-event-listener", "event_listener",
        frozenset({"default", "parking", "std"}), (),
        ("notify", "public_surface", "callback_contracts", "first_use",
         "backend_contention", "notification_policy", "panic_contracts",
         "blocking", "generic_contracts"),
    ),
    "listener-critical-section": CaptureGroup(
        "rca-event-listener", "dependencies/rca-event-listener", "event_listener",
        frozenset({"critical-section"}),
        ("--no-default-features", "--features", "critical-section"),
        ("notify", "public_surface", "callback_contracts", "first_use",
         "backend_contention", "notification_policy", "panic_contracts"),
    ),
    "strategy-std": CaptureGroup(
        "rca-event-listener-strategy", "dependencies/rca-event-listener-strategy",
        "event_listener_strategy", frozenset({"default", "std"}), (),
        ("easy_wrapper", "public_contracts"),
    ),
}
GIB = 1024 ** 3


def group_arguments(group):
    """Select the fixed package, feature set and ordinary test hosts."""
    if group not in TARGETS:
        raise ValueError(f"Unsupported coverage group: {group}")
    selected = TARGETS[group]
    options = ["-p", selected.package, *selected.cargo_options]
    if selected.include_library:
        options.append("--lib")
    return options + [item for name in selected.tests for item in ("--test", name)]


def host_arguments(group, kind, executable):
    """Serialize listener integration parents without filtering their tests."""
    args = [str(executable)]
    if group in ("listener-std", "listener-critical-section") and kind == ["test"]:
        args.append("--test-threads=1")
    return args


def digest(path):
    """Hash large objects without loading them into memory."""
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n")


def capture_environment(inherited, tools, target):
    """Allowlist execution inputs; never inherit secrets or compiler overrides."""
    env = {key: inherited[key] for key in ("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "TMPDIR") if key in inherited}
    env.update(PATH=str(tools["cargo"].parent) + os.pathsep + env.get("PATH", os.defpath),
               RUSTC=str(tools["rustc"]), RUSTDOC=str(tools["rustdoc"]),
               RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="", RUSTDOCFLAGS="",
               RUSTFLAGS=RUSTFLAGS, RUSTC_BOOTSTRAP="1", CARGO_BUILD_JOBS="1",
               CARGO_INCREMENTAL="0", CARGO_TARGET_DIR=str(target),
               CARGO_TERM_COLOR="never", LC_ALL="C", TZ="UTC")
    for profile in ("DEV", "TEST"):
        for setting, value in (("OPT_LEVEL", "0"), ("DEBUG", "0"),
                               ("DEBUG_ASSERTIONS", "true"), ("OVERFLOW_CHECKS", "true")):
            env[f"CARGO_PROFILE_{profile}_{setting}"] = value
    return env


def source_snapshot(root):
    """Bind the actual clean Git checkout, including every tracked lockfile."""
    env = {key: os.environ[key] for key in ("PATH", "HOME") if key in os.environ}
    def git(*args):
        return subprocess.check_output(["git", *args], cwd=root, env=env, timeout=30)
    if git("status", "--porcelain", "--untracked-files=all"):
        raise ValueError("Capture requires a clean Git checkout")
    names = [os.fsdecode(name) for name in git("ls-files", "-z").split(b"\0") if name]
    if "Cargo.lock" not in names:
        raise ValueError("Tracked Cargo.lock is required")
    files = {}
    for name in names:
        path = root / name
        if not path.is_file() or any(part.is_symlink() for part in (path, *path.parents)):
            raise ValueError(f"Tracked source is not a regular non-symlink file: {name}")
        files[name] = digest(path)
    return {"head": git("rev-parse", "HEAD").decode().strip(),
            "tree": git("rev-parse", "HEAD^{tree}").decode().strip(), "files": files}


def parse_artifacts(text, root, target, group):
    """Reject feature unification, foreign packages and unexpected test hosts."""
    selected = TARGETS[group]
    expected = set(selected.tests)
    if selected.include_library:
        expected.add(selected.library)
    hosts = {}
    for line in text.splitlines():
        record = json.loads(line)
        if record.get("reason") != "compiler-artifact" or record.get("executable") is None:
            continue
        name = record["target"]["name"]
        executable = Path(record["executable"])
        if (record["manifest_path"] != str(root / selected.directory / "Cargo.toml")
                or set(record["features"]) != selected.features or not record["profile"]["test"]
                or record["target"]["kind"] != (["lib"] if name == selected.library else ["test"])
                or executable.parent != target / "debug/deps" or not executable.is_file()
                or any(part.is_symlink() for part in (executable, *executable.parents))
                or name not in expected or name in hosts
                or str(executable) not in record["filenames"]):
            raise ValueError("Cargo artifact does not match the selected configuration")
        hosts[name] = record
    if set(hosts) != expected:
        raise ValueError("Missing selected test executables")
    return [hosts[name] for name in sorted(hosts)]


def test_summary(text, allow_exact_children=False):
    """Require a successful parent run; accept exact-filtered listener children only."""
    matches = re.findall(
        r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored; "
        r"(\d+) measured; (\d+) filtered out(?:; finished in [^\n]+)?$", text, re.MULTILINE)
    if not matches or (not allow_exact_children and len(matches) != 1):
        raise ValueError("Expected one successful libtest summary")
    summaries = [dict(zip(("passed", "failed", "ignored", "measured", "filtered_out"), map(int, match)))
                 for match in matches]
    summary = summaries[-1]
    if summary["passed"] == 0 or summary["failed"] != 0 or summary["filtered_out"] != 0:
        raise ValueError("Selected host did not run its positive, unfiltered test cohort")
    if allow_exact_children:
        summary_lines = re.findall(r"^test result: [^\n]+$", text, re.MULTILINE)
        total_tests = summary["passed"] + summary["ignored"] + summary["measured"]
        children = summaries[:-1]
        if (len(summary_lines) != len(summaries) or len(children) > summary["passed"]
                or any(child["passed"] != 1 or child["failed"] != 0
                       or child["ignored"] != 0 or child["measured"] != 0
                       or child["filtered_out"] != total_tests - 1 for child in children)):
            raise ValueError("Listener child did not finish one exact-filtered test")
        return dict(summary, exact_children=children)
    return summary


def run_command(argv, cwd, env, prefix, timeout, minimum_free=2 * GIB):
    """Retain process output/status, with bounded process-group and disk cleanup."""
    write_json(prefix.with_suffix(".argv.json"), list(map(str, argv)))
    status = {"started": time.time(), "returncode": None, "stop_reason": None}
    process = None
    try:
        if shutil.disk_usage(prefix.parent).free < minimum_free:
            status["stop_reason"] = "low disk"
            raise ValueError("Insufficient free disk space")
        with prefix.with_suffix(".stdout").open("wb") as stdout, prefix.with_suffix(".stderr").open("wb") as stderr:
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            deadline = time.monotonic() + timeout
            while process.poll() is None:
                if shutil.disk_usage(prefix.parent).free < minimum_free:
                    status["stop_reason"] = "low disk"
                    break
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    status["stop_reason"] = "timeout"
                    break
                try:
                    process.wait(timeout=min(1, remaining))
                except subprocess.TimeoutExpired:
                    pass
    finally:
        if process is not None:
            cleanup = process.poll() is None
            if not cleanup:
                try:
                    os.killpg(process.pid, 0)
                except ProcessLookupError:
                    pass
                else:
                    status["stop_reason"] = status["stop_reason"] or "remaining descendants"
                    cleanup = True
            if cleanup:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
                # The leader can exit on TERM while a descendant ignores it.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
            status["returncode"] = process.returncode
        status["finished"] = time.time()
        write_json(prefix.with_suffix(".status.json"), status)
    if status["stop_reason"] or status["returncode"] != 0:
        raise ValueError(f"Command failed; see {prefix}.status.json")


def validate_tool_versions(versions):
    """Require one pinned Rust release and its matching branch-coverage tools."""
    fields = dict(line.split(": ", 1) for line in versions["rustc"].splitlines() if ": " in line)
    if (not versions["rustc"].startswith("rustc 1.99.0 ")
            or fields.get("release") != TOOLCHAIN
            or not fields.get("host", "").endswith("-unknown-linux-gnu")
            or fields.get("LLVM version") != "23.1.1"
            or not versions["cargo"].startswith("cargo 1.99.0 ")
            or not versions["rustdoc"].startswith("rustdoc 1.99.0 ")
            or any("LLVM version 23.1.1-rust-1.99.0-stable" not in versions[name]
                   for name in ("llvm-profdata", "llvm-cov"))):
        raise ValueError("Coverage requires Rust 1.99.0 Linux GNU and its LLVM 23.1.1 tools")


def discover_tools(root, output):
    """Resolve explicitly through rustup; retain versions and identities."""
    env = {key: os.environ[key] for key in ("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME") if key in os.environ}
    rustup = shutil.which("rustup", path=env.get("PATH"))
    if rustup is None:
        raise ValueError("rustup is required")
    tools = {"rustup": Path(rustup).resolve()}
    versions = {}
    for name in ("cargo", "rustc", "rustdoc"):
        run_command([rustup, "which", "--toolchain", TOOLCHAIN, name], root, env, output / ("which-" + name), 30)
        tools[name] = Path((output / ("which-" + name + ".stdout")).read_text().strip())
        if not tools[name].is_absolute() or not tools[name].is_file():
            raise ValueError("rustup returned a missing tool")
        run_command([str(tools[name]), "-Vv" if name == "rustc" else "--version"], root, env, output / ("version-" + name), 30)
        versions[name] = (output / ("version-" + name + ".stdout")).read_text()
    if len({tools[name].parent for name in ("cargo", "rustc", "rustdoc")}) != 1:
        raise ValueError("Rust tools came from different installations")
    fields = dict(line.split(": ", 1) for line in versions["rustc"].splitlines() if ": " in line)
    host = fields.get("host", "")
    if not host.endswith("-unknown-linux-gnu") or "/" in host:
        raise ValueError("Unsupported Rust host")
    llvm = tools["rustc"].parent.parent / "lib/rustlib" / host / "bin"
    for name in ("llvm-profdata", "llvm-cov", "cc", "ld"):
        path = llvm / name if name.startswith("llvm-") else Path(shutil.which(name, path=env.get("PATH")) or "/missing")
        if not path.is_file():
            raise ValueError(f"Missing required tool: {name}")
        tools[name] = path
        run_command([str(path), "--version"], root, env, output / ("version-" + name), 30)
        versions[name] = (output / ("version-" + name + ".stdout")).read_text()
    validate_tool_versions(versions)
    return tools


def capture(root, group, output):
    """Capture a fixed cohort; leave partial evidence on failure."""
    if sys.platform != "linux":
        raise ValueError("Coverage capture is restricted to isolated Linux runners")
    options = group_arguments(group)
    root = root.resolve()
    output = output if output.is_absolute() else root / output
    target = root / "target/coverage" / group
    for path in (output, target):
        if any(part.is_symlink() for part in (path, *path.parents)):
            raise ValueError("Coverage paths must not traverse symlinks")
    output = output.resolve()
    if output.is_relative_to(target) or target.is_relative_to(output):
        raise ValueError("Evidence must be separate from the reusable Cargo target")
    if output.exists():
        raise ValueError("Coverage output must be a fresh directory")
    output.mkdir(parents=True)
    receipt = {"group": group, "capture_complete": False, "coverage_complete": False,
               "target": str(target), "started": time.time(),
               "metadata": {key: os.environ[key] for key in (
                   "GITHUB_SHA", "GITHUB_HEAD_REF", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion") if key in os.environ}}
    before = tools = identities = None
    failure = None
    try:
        receipt["initial_free_bytes"] = shutil.disk_usage(output).free
        if receipt["initial_free_bytes"] < 12 * GIB:
            raise ValueError("Coverage requires 12 GiB free for target and retained evidence")
        before = source_snapshot(root)
        write_json(output / "source-before.json", before)
        if tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"] != TOOLCHAIN:
            raise ValueError("Repository toolchain differs from the coverage toolchain")
        tools = discover_tools(root, output)
        identities = {name: {"path": str(path), "resolved": str(path.resolve()), "sha256": digest(path)} for name, path in tools.items()}
        write_json(output / "tools-before.json", identities)
        target.mkdir(parents=True, exist_ok=True)
        env = capture_environment(os.environ, tools, target)
        write_json(output / "environment.json", env)
        # Capture external Cargo configuration identities, never their contents.
        configs = {path for parent in (root, *root.parents) for path in (parent / ".cargo/config", parent / ".cargo/config.toml")}
        cargo_home = Path(env.get("CARGO_HOME", str(Path(env["HOME"]) / ".cargo")))
        configs.update((cargo_home / "config", cargo_home / "config.toml"))
        config_before = {str(path): digest(path) for path in configs if path.is_file()}
        write_json(output / "cargo-config-before.json", config_before)
        build_raw = output / "build-raw"
        build_raw.mkdir()
        env["LLVM_PROFILE_FILE"] = str(build_raw / "current-%p-%m.profraw")
        run_command([str(tools["cargo"]), "test", "--locked", "-j1", "--message-format=json-render-diagnostics",
                     *options, "--no-run"], root, env, output / "build", 1800)
        hosts = parse_artifacts((output / "build.stdout").read_text(), root, target, group)
        write_json(output / "compiler-artifacts.json", hosts)
        objects = output / "objects"
        objects.mkdir()
        for record in hosts:
            host = output / "hosts" / record["target"]["name"]
            raw = host / "raw"
            raw.mkdir(parents=True)
            original = Path(record["executable"])
            retained = objects / original.name
            shutil.copy2(original, retained)
            if digest(original) != digest(retained):
                raise ValueError("Retained object differs from Cargo executable")
            write_json(host / "object.json", {"original": str(original), "retained": str(retained), "sha256": digest(retained)})
            package = Path(record["manifest_path"]).parent
            host_env = dict(env, CARGO_MANIFEST_DIR=str(package),
                            LLVM_PROFILE_FILE=str(raw / "current-%p-%m.profraw"),
                            LD_LIBRARY_PATH=os.pathsep.join((str(tools["llvm-cov"].parent.parent / "lib"), str(tools["rustc"].parent.parent / "lib"))))
            write_json(host / "environment.json", host_env)
            run_command(host_arguments(group, record["target"]["kind"], retained),
                        package, host_env, host / "run", 600)
            listener_child = (group in ("listener-std", "listener-critical-section")
                              and record["target"]["kind"] == ["test"])
            write_json(host / "test-summary.json", test_summary((host / "run.stdout").read_text(), listener_child))
            profiles = sorted(raw.glob("*.profraw"))
            if not profiles or set(raw.iterdir()) != set(profiles) or any(path.is_symlink() for path in profiles):
                raise ValueError("Missing or unexpected runtime profiles")
            write_json(host / "raw-sha256.json", {path.name: digest(path) for path in profiles})
            run_command([str(tools["llvm-profdata"]), "merge", "-sparse", *map(str, profiles), "-o", str(host / "runtime.profdata")],
                        package, env, host / "merge", 120)
            run_command([str(tools["llvm-cov"]), "export", "-format=text", "-instr-profile=" + str(host / "runtime.profdata"), "-object", str(retained)],
                        package, env, host / "export", 120)
            if (host / "merge.stderr").stat().st_size or (host / "export.stderr").stat().st_size:
                raise ValueError("LLVM emitted diagnostics; native evidence is not admitted")
            with (host / "export.stdout").open("rb") as stream:
                data = json.load(stream)
                if not isinstance(data.get("data"), list) or not data["data"]:
                    raise ValueError("LLVM export has no coverage data")
            with (host / "export.stdout").open("rb") as stream, (host / "export.json.gz").open("wb") as destination:
                with gzip.GzipFile(filename="", mode="wb", fileobj=destination, mtime=0) as compressed:
                    shutil.copyfileobj(stream, compressed)
            (host / "export.stdout").unlink()  # Full bytes remain in export.json.gz.
        config_after = {str(path): digest(path) for path in configs if path.is_file()}
        write_json(output / "cargo-config-after.json", config_after)
        if config_before != config_after:
            raise ValueError("Cargo configuration changed during capture")
    except (Exception, KeyboardInterrupt) as error:
        failure = error
    finally:
        try:
            if before is not None:
                after = source_snapshot(root)
                write_json(output / "source-after.json", after)
                if after != before:
                    raise ValueError("Tracked source changed during capture")
            if tools is not None and identities is not None:
                after_tools = {name: {"path": str(path), "resolved": str(path.resolve()), "sha256": digest(path)} for name, path in tools.items()}
                write_json(output / "tools-after.json", after_tools)
                if after_tools != identities:
                    raise ValueError("Tool identity changed during capture")
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            receipt["postcheck_error"] = str(error)
            failure = failure or error
        receipt.update(finished=time.time(), capture_complete=failure is None)
        if failure is not None:
            receipt["error"] = str(failure)
        write_json(output / "capture.json", receipt)
        write_json(output / "sha256.json", {str(path.relative_to(output)): digest(path) for path in sorted(output.rglob("*")) if path.is_file()})
    if failure is not None:
        raise ValueError(f"Coverage capture failed; see {output / 'capture.json'}") from failure
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--group", required=True, choices=TARGETS)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    capture(root, args.group, args.output)
    print("Native evidence retained; production coverage has not been adjudicated.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
