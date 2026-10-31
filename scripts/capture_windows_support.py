#!/usr/bin/env python3
"""Capture Windows support measurements; missing CPU profiles remain incomplete.

Run inside the reviewed outer Windows memory guard. The existing Windows process
owner gives each child tree a 2 GiB limit and closes owned compiler telemetry.
This does not replace the paired runtime gates or provide instruction counts.
"""

import argparse
from contextlib import ExitStack
import ctypes
import json
import ntpath
import os
from pathlib import Path
import struct
import sys
import time

import composition_runner as runner
from windows_process import WindowsProcessApi, validate_arguments, wait_for_empty_job


provider_module = runner.windows_provider()
MEMORY_LIMIT = 2 * 1024**3
BENCHES = (("rca-core", "effects"), ("rca-core", "observation"),
           ("rca-core", "store_actor"), ("rca-test", "deterministic"),
           ("rca-tokio", "runtime"))
CONTENTION = tuple(f"external_commit/multi/{count}_producers/saturated" for count in (4, 8))


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def file_record(path):
    """Bind a retained regular file to its bytes, without accepting links."""
    path = Path(path)
    if path.is_symlink() or not path.is_file() or not path.stat().st_size:
        raise ValueError(f"missing, empty or linked evidence file: {path}")
    return {"path": str(path), "sha256": provider_module.sha256(path), "size_bytes": path.stat().st_size}


def checked_file(record):
    actual = file_record(record["path"])
    if actual["sha256"] != record["sha256"]:
        raise ValueError(f"file hash changed: {record['path']}")
    return actual


def verify_source(source, manifest):
    """Require the entire immutable stage, not just its Rust files, to match."""
    if source.is_symlink() or not source.is_dir():
        raise ValueError("source must be an immutable directory")
    if any(path.is_symlink() for path in source.rglob("*")):
        raise ValueError("source contains a link")
    if not manifest or runner.source_hashes(source) != manifest:
        raise ValueError("source differs from the accepted staged manifest")


def build_environment(inherited, tools, target):
    """Keep compiler inputs explicit and reject inherited measurement overrides."""
    forbidden = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
                 "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTDOCFLAGS", "CARGO_BUILD_RUSTFLAGS",
                 "CARGO_BUILD_RUSTC", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
                 "CARGO_BUILD_TARGET", "RUSTC_BOOTSTRAP", "LLVM_PROFILE_FILE",
                 "RCA_QUALIFY_ALLOCATIONS", "RCA_DHAT_FILE"}
    for key, value in inherited.items():
        if value and (key.upper() in forbidden or key.upper().startswith(("CARGO_PROFILE_", "DIVAN_"))
                      or key.upper().startswith("CARGO_TARGET_") and key.upper().endswith("_RUSTFLAGS")):
            raise ValueError(f"measurement environment override: {key}")
    env = dict(inherited)
    # Windows environment names are case-insensitive. Replace, do not duplicate.
    explicit = {"RUSTC": tools["rustc"]["path"], "RUSTDOC": tools["rustdoc"]["path"],
                "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "", "CARGO_BUILD_JOBS": "1",
                "CARGO_INCREMENTAL": "0", "CARGO_TARGET_DIR": str(target), "RUSTUP_TOOLCHAIN": "1.99.0"}
    env = {key: value for key, value in env.items() if key.upper() not in explicit}
    env.update(explicit)
    path_key = next((key for key in env if key.upper() == "PATH"), "PATH")
    env[path_key] = str(Path(tools["cargo"]["path"]).parent) + os.pathsep + env.get(path_key, "")
    return env


def validate_inputs(source, manifest_path, metadata_path, tools_path, normal, allocation, revision):
    """Bind source, tools and existing workload binaries to one successful build."""
    metadata = read_json(metadata_path)
    provider_module.validate_source_metadata(metadata)
    if (metadata["candidate"]["revision"] != revision or metadata.get("build_exit_code") != 0
            or metadata.get("profile_environment") != {}):
        raise ValueError("candidate build identity or clean profile environment is unverified")
    manifest = read_json(manifest_path)
    if (provider_module.sha256(manifest_path) != metadata.get("candidate_source_manifest_sha256")
            or metadata.get("post_build_source_verification", {}).get("candidate")
            != {"files": len(manifest), "exact_manifest_match": True}):
        raise ValueError("candidate source manifest was not verified after its build")
    verify_source(source, manifest)
    if (manifest.get("Cargo.lock") != metadata["candidate"]["lock_sha256"]
            or manifest.get("Cargo.toml") != metadata["candidate"]["manifest_sha256"]
            or manifest.get("rca-tokio/benches/qualification.rs") != metadata["harness_sha256"]
            or manifest.get("docs/performance/scenarios.txt") != metadata["scenario_manifest_sha256"]):
        raise ValueError("staged workspace differs from the qualification build inputs")
    for name, path in (("candidate_normal", normal), ("candidate_allocation", allocation)):
        if file_record(path)["sha256"] != metadata["binaries"][name]["sha256"]:
            raise ValueError(f"{name} differs from its source-build receipt")
    tools = read_json(tools_path)
    for name in ("cargo", "rustc", "rustdoc", "objcopy", "compiler_telemetry"):
        checked_file(tools[name])
        if not ntpath.isabs(tools[name]["path"]) or not tools[name]["path"].lower().endswith(".exe"):
            raise ValueError(f"{name} needs an absolute Windows executable")
        if name != "objcopy" and metadata["input_hashes"].get(tools[name]["path"]) != tools[name]["sha256"]:
            raise ValueError(f"{name} differs from the pinned source-build tool")
    return metadata, manifest, tools


def stripped_pe_size(path):
    """Reject raw executable sizes when PE debug or COFF symbol tables remain."""
    data = path.read_bytes()
    try:
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        machine, _, _, symbols, count, optional_size, _ = struct.unpack_from("<HHIIIHH", data, pe + 4)
        optional = pe + 24
        magic = struct.unpack_from("<H", data, optional)[0]
        directories = struct.unpack_from("<I", data, optional + 108)[0]
        debug = struct.unpack_from("<II", data, optional + 112 + 6 * 8)
    except struct.error as error:
        raise ValueError("truncated PE executable") from error
    if (data[:2] != b"MZ" or data[pe:pe + 4] != b"PE\0\0" or machine != 0x8664
            or magic != 0x20B or optional_size < 168 or directories < 7
            or symbols or count or debug != (0, 0)):
        raise ValueError("executable is not a verified stripped x64 PE image")
    return len(data)


def bench_artifact(transcript, name, target):
    """Select exactly the requested executable from Cargo's artifact stream."""
    matches = []
    for line in transcript.read_text(encoding="utf-8").splitlines():
        if not line.startswith("{"):
            continue
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == name
                and item["target"].get("kind") == ["bench"] and item.get("executable")):
            matches.append(Path(item["executable"]))
    if len(matches) != 1:
        raise ValueError(f"expected exactly one {name} bench executable")
    path = matches[0]
    if not path.resolve().is_relative_to(target.resolve()) or path.is_symlink() or not path.is_file():
        raise ValueError("bench executable is outside the owned target")
    return path


def live_working_set(provider, process):
    """Read current resident bytes from a still-running process, never its peak."""
    memory = provider_module.ProcessMemoryCounters()
    memory.cb = ctypes.sizeof(memory)
    if not provider.psapi.GetProcessMemoryInfo(process, ctypes.byref(memory), ctypes.sizeof(memory)):
        raise OSError("GetProcessMemoryInfo failed for idle process")
    if memory.WorkingSetSize <= 0:
        raise ValueError("idle live working set is missing")
    return memory.WorkingSetSize


def capture_idle(executable, destination, source, env, provider, *, api=None, clock=time.monotonic, sleep=time.sleep):
    """Wait for READY, sample a live store, then terminate only its owned job."""
    command = [str(executable), "--idle"]
    validate_arguments(command, str(source), env, 10)
    api = WindowsProcessApi() if api is None else api
    receipt = {"command": command, "job_memory_limit_bytes": MEMORY_LIMIT,
               "descendants_exited_naturally": False, "instructions_retired": None}

    def record_failure(error_type, error, traceback):
        if error is not None:
            error.windows_process_receipt = dict(receipt)

    with destination.open("xb") as transcript, ExitStack() as cleanup:
        cleanup.push(record_failure)
        job = api.create_job(MEMORY_LIMIT)
        cleanup.callback(api.close, job)
        process = api.create_process(command, transcript, str(source), env)
        cleanup.callback(api.close, process.process)
        cleanup.callback(api.close, process.thread)
        assigned = False
        try:
            api.assign(job, process.process)
            assigned = True
            api.resume(process.thread)
            deadline = clock() + 10
            while True:
                if api.wait(process.process, 0):
                    raise ValueError("idle process exited before the live sample")
                if "READY" in destination.read_text(encoding="utf-8").splitlines():
                    break
                if clock() >= deadline:
                    raise TimeoutError("idle Store did not become ready")
                sleep(0.1)
            receipt["readiness"] = "READY"
            sleep(1)
            if api.wait(process.process, 0):
                raise ValueError("idle process exited before the live sample")
            receipt["WorkingSetSize"] = live_working_set(provider, process.process)
            if api.wait(process.process, 0):
                raise ValueError("idle process exited during the live sample")
            receipt["stop_cause"] = "intentional-owned-job-stop-after-live-sample"
        finally:
            if assigned:
                api.terminate_job(job)
            else:
                api.terminate_process(process.process)
            deadline = clock() + 10
            if not api.wait(process.process, 10):
                raise OSError("idle process did not stop after owned termination")
            if assigned:
                wait_for_empty_job(api, job, deadline, clock, sleep, command, 10)
            receipt["descendants_exited"] = True
    return receipt


def capture(args):
    """Run the existing support workloads and retain every raw measurement."""
    if args.source.is_symlink():
        raise ValueError("source stage must not be a link")
    source = args.source.resolve()
    output = args.output.resolve()
    if output.is_relative_to(source) or source.is_relative_to(output):
        raise ValueError("output and immutable source must be separate directories")
    metadata, manifest, tools = validate_inputs(source, args.source_manifest, args.metadata, args.tools,
                                               args.candidate_normal, args.candidate_allocation,
                                               args.candidate_revision)
    controllers = [file_record(Path(path)) for path in (
        __file__, runner.__file__, runner.evidence.__file__, provider_module.__file__,
        sys.modules[WindowsProcessApi.__module__].__file__)]
    inputs = [file_record(path) for path in (args.metadata, args.source_manifest, args.tools)] + controllers
    # Validate inherited settings before creating any build target or process.
    base_env = build_environment(dict(os.environ), tools, output / "compile-target")
    host = runner.windows_host()
    output.mkdir(parents=False, exist_ok=False)
    protocol = {"process_timeout_seconds": 3600, "process_memory_limit_bytes": MEMORY_LIMIT,
                "compiler_telemetry": tools["compiler_telemetry"]}
    monitor = runner.WindowsMonitor(output, protocol, cargo=tools["cargo"]["path"], complete=True)
    result = {"method": "windows-runtime-support-v1", "status": "incomplete", "host": host,
              "started_at": runner.now(), "toolchain": metadata["toolchain"],
              "candidate": metadata["candidate"], "source_build": file_record(args.metadata),
              "source_manifest": file_record(args.source_manifest), "tools": tools, "controllers": controllers,
              "instructions_retired": None, "remaining": []}
    stage = "clean-workspace-build"
    try:
        cargo = tools["cargo"]["path"]
        compile_log = output / "clean-compile.txt"
        monitor.run([cargo, "build", "--locked", "--workspace", "--all-features", "--features",
                     "critical-section/std", "--release"], compile_log, source, base_env)
        if "stripping debug info" in compile_log.read_text(encoding="utf-8") and "failed" in compile_log.read_text(encoding="utf-8"):
            raise ValueError("clean release build reported a stripping failure")
        result["clean_compile"] = {"transcript": file_record(compile_log),
                                   "resources": read_json(compile_log.with_suffix(".resources.json"))}

        stage = "stripped-binary-sizes"
        consumer_env = build_environment(dict(os.environ), tools, output / "consumer-target")
        monitor.run([cargo, "build", "--locked", "-p", "rca-size-probe", "--release"],
                    output / "consumer-build.txt", source, consumer_env)
        result["stripped_binaries"] = {}
        for name, original in (("qualification", args.candidate_normal),
                               ("rca-size-probe", output / "consumer-target/release/rca-size-probe.exe")):
            stripped = output / f"{name}-stripped.exe"
            monitor.run([tools["objcopy"]["path"], "--strip-all", str(original), str(stripped)],
                        output / f"{name}-strip.txt", source, base_env)
            result["stripped_binaries"][name] = {"original": file_record(original),
                "stripped": file_record(stripped), "stripped_size_bytes": stripped_pe_size(stripped)}

        stage = "idle-live-memory"
        result["idle_store"] = capture_idle(args.candidate_normal, output / "idle-ready.txt", source,
                                            base_env, monitor.provider)
        runner.write_json(output / "idle-memory.json", result["idle_store"])

        stage = "divan-suites"
        result["divan"] = {}
        bench_target = output / "bench-target"
        bench_env = build_environment(dict(os.environ), tools, bench_target)
        for package, bench in BENCHES:
            build_log = output / f"{bench}-build.jsonl"
            monitor.run([cargo, "bench", "--locked", "-p", package, "--bench", bench,
                         "--no-run", "--message-format=json"], build_log, source, bench_env)
            executable = bench_artifact(build_log, bench, bench_target)
            transcript = output / f"{bench}-divan.txt"
            monitor.run([str(executable), "--bench", "--color", "never", "--sample-count", "50",
                         "--max-time", "0.5"], transcript, source, bench_env)
            rows = runner.evidence.parse_divan_transcript(transcript)
            result["divan"][bench] = {"package": package, "executable": file_record(executable),
                                      "transcript": file_record(transcript), "rows": rows}

        stage = "asynchronous-effect-dhat"
        heap = output / "effect-async-dhat.json"
        heap_env = {**base_env, "RCA_QUALIFY_ALLOCATIONS": "1", "RCA_DHAT_FILE": str(heap)}
        heap_log = output / "effect-async-dhat.txt"
        monitor.run([str(args.candidate_allocation), "effect/async", "10000"], heap_log, source, heap_env)
        measurements = [json.loads(line) for line in heap_log.read_text(encoding="utf-8").splitlines()
                        if line.startswith("{")]
        if len(measurements) != 1:
            raise ValueError("DHAT workload must return exactly one measurement")
        provider_module.validate_measurement(measurements[0], "effect/async", 10000, "allocation")
        heap_data = read_json(heap)
        if heap_data.get("dhatFileVersion") != 2 or not heap_data.get("pps"):
            raise ValueError("DHAT heap profile is missing allocation records")
        result["dhat"] = {"profile": file_record(heap), "transcript": file_record(heap_log),
                          "measurement": measurements[0]}

        stage = "contention-cpu-profiles"
        # The reviewed WPR producer can attach its evidence later. Trace hashes
        # alone do not prove that CPU samples belong to these workload processes.
        result["cpu_profiles"] = {"status": "pending", "required_commands": [
            [str(args.candidate_normal), scenario, "100000"] for scenario in CONTENTION],
            "binary": file_record(args.candidate_normal)}
        result["remaining"].append("Two actual WPR contention captures and process-attributed CPU-sample analysis are required.")
        stage = "closing-input-verification"
        for record in inputs:
            checked_file(record)
        verify_source(source, manifest)
        validate_inputs(source, args.source_manifest, args.metadata, args.tools,
                        args.candidate_normal, args.candidate_allocation, args.candidate_revision)
        runner.check_host_unchanged(host, monitor)
        result["non_profile_support_captured"] = True
        result["finished_at"] = runner.now()
        runner.write_json(output / "support.json", result)
        return result
    except BaseException as error:
        result.update(failed_stage=stage, error=str(error))
        if hasattr(error, "windows_process_receipt"):
            result["windows_process_receipt"] = error.windows_process_receipt
        runner.write_json(output / "support.incomplete.json", result)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source", "source-manifest", "metadata", "tools", "candidate-normal",
                 "candidate-allocation", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--candidate-revision", required=True)
    args = parser.parse_args()
    if sys.platform != "win32":
        parser.error("native support capture requires Windows")
    result = capture(args)
    print(json.dumps({"status": result["status"], "remaining": result["remaining"],
                      "non_profile_support_captured": result["non_profile_support_captured"],
                      "cpu_profiles": result["cpu_profiles"]["status"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
