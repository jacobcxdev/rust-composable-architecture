"""Observe removal of a child with 10,000 ready tasks on Windows.

The sample count applies to complete fixtures, not individual tasks. The wall
window includes setup and cleanup. The reported median times the removal only.
The caller supplies the verified benchmark build and compares independent runs.
No result from this module qualifies a release on its own.
"""

import argparse
import ctypes
import json
import math
import os
from pathlib import Path, PureWindowsPath
import re
import shutil
import subprocess
import sys
import tempfile
import time

import composition_performance as evidence


SCENARIO = "descendant_cancellation/10000"
MIN_SAMPLES = 1_024
TIMEOUT = 40
_JOB_BYTES = 2 * 1024**3
_ARGS = (
    "--bench", "--exact", "composition::descendant_cancellation::10000",
    "--skip-ext-time", "false", "--min-time", "2",
    "--sample-count", str(MIN_SAMPLES), "--sample-size", "1", "--max-time", "32",
    "--color", "never",
)


def replay(transcript, events, *, executable, cwd, destination, witnessed=False):
    """Read one sealed transcript, its resource file and its two process events.

    The caller checks the capture's file hashes before replay. Supply the
    original Windows paths, even if the files are being read on another host.
    """
    paths = [PureWindowsPath(p) for p in (executable, cwd, destination)]
    if not all(p.is_absolute() and ".." not in p.parts for p in paths):
        raise ValueError("cancellation exposure requires absolute Windows paths")
    if transcript.name != paths[-1].name:
        raise ValueError("cancellation transcript differs from its recorded destination")
    events = [event for event in events if event.get("destination") == str(destination)]
    if [e.get("phase") for e in events] != ["start", "finish"]:
        raise ValueError("missing or repeated cancellation exposure process")
    start, finish = events
    resource = json.loads(transcript.with_suffix(".resources.json").read_bytes())
    evidence.validate_windows_resource_counters(
        resource, expected_kind="runtime", primary_thread_affinity_required=True
    )
    receipt = resource["process_receipt"]
    if (start.get("command") != [str(executable), *_ARGS] or start.get("cwd") != str(cwd)
        or start.get("command_kind") != "runtime" or start.get("timeout_seconds") != TIMEOUT
        or start.get("job_memory_limit_bytes") != _JOB_BYTES
        or finish.get("returncode") != 0 or type(finish.get("returncode")) is not int
        or finish.get("job_memory_limit_bytes") != _JOB_BYTES
        or finish.get("process_receipt") != receipt or "error" in finish
        or receipt.get("divan_arguments") != list(_ARGS)):
        raise ValueError("cancellation exposure command or process receipt differs")
    direct = resource["direct_process"]
    lifetime = (direct["ExitTime100ns"] - direct["CreationTime100ns"]) / 10_000_000
    if not 2 <= lifetime <= TIMEOUT or not 2 <= receipt["leader_elapsed_seconds"] <= TIMEOUT:
        raise ValueError("cancellation process lifetime is outside the wall window")

    text = transcript.read_text(encoding="utf-8")
    if witnessed:
        # The deliberate regression prints milestones on stderr. Windows merges
        # it with stdout, sometimes in the middle of the Divan leaf.
        marker = r"descendant-cancellation-call=(\d+)\n"
        if re.findall(marker, text) != ["1", "256", "1024"]:
            raise ValueError("missing or unexpected cancellation fault witness")
        text = re.sub(marker, "", text)
        with tempfile.TemporaryDirectory() as scratch:
            timing = Path(scratch) / "timing.txt"
            timing.write_text(text, encoding="utf-8")
            median = evidence.validate_isolated_transcript(timing, SCENARIO)[SCENARIO]
    else:
        median = evidence.validate_isolated_transcript(transcript, SCENARIO)[SCENARIO]
    # Read the count from the same Divan leaf, never from a summary or another line.
    leaves = [line for line in text.splitlines()
              if line.startswith("   ╰─ 10000 ")]
    if len(leaves) != 1:
        raise ValueError("missing or ambiguous cancellation sample count")
    columns = leaves[0].split("│")
    if len(columns) != 6 or not all(re.fullmatch(r"[0-9]+", c.strip()) for c in columns[-2:]):
        raise ValueError("missing or invalid cancellation sample count")
    samples, iterations = map(int, columns[-2:])
    if samples != iterations or samples < MIN_SAMPLES:
        raise ValueError("cancellation exposure needs at least 1,024 samples of one operation")
    return {"samples": samples, "median_ns": median, "process_seconds": lifetime}


def capture(monitor, executable, cwd, transcript, env, *, executable_sha256, witnessed=False):
    """Run one fresh observation with the selected executable and Windows monitor."""
    from composition_runner import WindowsMonitor

    if (not isinstance(monitor, WindowsMonitor)
        or monitor.protocol.get("method_version") != evidence.WINDOWS_V6_METHOD
        or monitor.protocol.get("process_memory_limit_bytes") != _JOB_BYTES):
        raise ValueError("cancellation exposure requires the Windows v6 process guard")
    executable, cwd, transcript = map(Path, (executable, cwd, transcript))
    if (any(not p.is_absolute() or p.resolve() != p for p in (executable, cwd, transcript))
        or not cwd.is_dir() or transcript.suffix != ".txt"
        or transcript.exists() or transcript.with_suffix(".resources.json").exists()):
        raise ValueError("cancellation exposure requires exact paths and a fresh output")
    if any(key.upper().startswith(("DIVAN_", "RCA_COMPOSITION_ALLOCATION_", "RCA_SCOPED_ALLOCATION_"))
           for key in env):
        raise ValueError("cancellation exposure does not accept ambient benchmark options")
    if evidence._sha256(executable) != executable_sha256:
        raise ValueError("cancellation benchmark executable does not match its build")
    monitor.run([str(executable), *_ARGS], transcript, cwd, env, timeout=TIMEOUT)
    if evidence._sha256(executable) != executable_sha256:
        raise ValueError("cancellation benchmark executable changed during measurement")
    events = (json.loads(line) for line in (monitor.directory / "processes.jsonl").read_bytes().splitlines())
    return replay(transcript, events, executable=executable, cwd=cwd, destination=transcript,
                  **({"witnessed": True} if witnessed else {}))


# Each comparison has its own reference processes. Reverse both pair orders in
# the second block. No inner Divan iteration counts as an independent process.
_ROLES = ("candidate", "reference", "control", "reference",
          "reference", "control", "reference", "candidate")


def screen_result(rows, limit):
    """Classify removal medians only. A screen pass is not release clearance."""
    if len(rows) != len(_ROLES) or not math.isfinite(limit) or limit <= 1:
        raise ValueError("invalid cancellation screen schedule or limit")
    times = [r["median_ns"] for r in rows]
    if any(not math.isfinite(ns) or ns <= 0 for ns in times):
        raise ValueError("invalid removal time")
    if any(type(r["samples"]) is not int or r["samples"] < MIN_SAMPLES for r in rows):
        raise ValueError("incomplete cancellation exposure")
    ratios = [times[0] / times[1], times[7] / times[6]]
    controls = [times[2] / times[3], times[5] / times[4]]
    if not all(1 / limit <= ratio <= limit for ratio in controls):
        verdict = "inconclusive"
    elif all(ratio > limit for ratio in ratios):
        verdict = "fail"
    elif all(ratio <= limit for ratio in ratios):
        verdict = "pass"
    else:
        verdict = "inconclusive"
    return {"verdict": verdict, "scope": "cancellation-removal-median-pr-screen",
            "release_authorised": False, "ratio_limit": limit,
            "candidate_ratios": ratios, "unchanged_control_ratios": controls}


def capture_screen(monitor, builds, cwd, directory, env, *, witnessed_candidate=False):
    """Run one bounded screen inside the admitted Windows job and exclusive machine lock.

    Supply (path, SHA-256) for each verified build. The caller must check source,
    workload and compiler provenance, including two independently built references.
    A different memory address or file name does not establish a separate build.
    """
    if set(builds) != set(_ROLES):
        raise ValueError("expected candidate, reference and independent control builds")
    images = {}
    for role, (path, digest) in builds.items():
        path = Path(path)
        if (not path.is_absolute() or path.resolve(strict=True) != path
            or not re.fullmatch(r"[0-9a-f]{64}", digest) or evidence._sha256(path) != digest):
            raise ValueError("invalid cancellation screen image")
        images[role] = {"path": str(path), "sha256": digest}
    if Path(builds["control"][0]).samefile(builds["reference"][0]):
        raise ValueError("the two control builds must be separate")
    if monitor.protocol.get("regression_limit") != 1.10:
        raise ValueError("unexpected Windows regression limit")
    directory = Path(directory)
    directory.mkdir(exist_ok=False)
    started = time.monotonic()
    rows = []
    for index, role in enumerate(_ROLES):
        path, digest = builds[role]
        rows.append(capture(monitor, path, cwd, directory / f"{index}-{role}.txt", env,
                            executable_sha256=digest, witnessed=witnessed_candidate and role == "candidate"))
    result = screen_result(rows, monitor.protocol["regression_limit"])
    return {**result, "images": images, "measurement_seconds": time.monotonic() - started,
            "rows": rows}


def replay_screen(directory, journal, *, cwd, destination, images, witnessed_candidate=False):
    """Recompute a screen from the eight original process records.

    Supply independently checked build paths and the original Windows locations.
    The caller verifies file digests and the enclosing capture's host and guard.
    """
    destination = PureWindowsPath(destination)
    if set(images) != set(_ROLES) or PureWindowsPath(images["control"]) == PureWindowsPath(images["reference"]):
        raise ValueError("missing separate control build paths")
    events = [json.loads(line) for line in journal.read_bytes().splitlines()]
    schedule = [(phase, str(destination / f"{i}-{role}.txt")) for i, role in enumerate(_ROLES)
                for phase in ("start", "finish")]
    if [(e.get("phase"), e.get("destination")) for e in events] != schedule:
        raise ValueError("journal does not contain exactly the eight scheduled processes")
    rows = []
    previous_exit = 0
    for index, role in enumerate(_ROLES):
        filename = f"{index}-{role}.txt"
        transcript = directory / filename
        row = replay(transcript, events, executable=images[role], cwd=cwd,
                     destination=destination / filename, witnessed=witnessed_candidate and role == "candidate")
        counters = json.loads(transcript.with_suffix(".resources.json").read_bytes())["direct_process"]
        if counters["CreationTime100ns"] < previous_exit:
            raise ValueError("overlapping or out-of-order cancellation processes")
        previous_exit = counters["ExitTime100ns"]
        rows.append(row)
    return {**screen_result(rows, 1.10), "process_seconds": sum(r["process_seconds"] for r in rows),
            "rows": rows}


def build_bindings(path, expected_sha256):
    """Load a separately pinned, operator-reviewed binding from source builds to images.

    Before pinning this file, verify each source revision, build receipt and
    executable hash, and match workload, compiler, linker, flags and target.
    The PR screen measures those images; it cannot establish build provenance.
    """
    import hashlib
    raw = path.read_bytes()
    if hashlib.sha256(raw).hexdigest() != expected_sha256:
        raise ValueError("build bindings differ from the independent pin")
    data = json.loads(raw)
    if (not isinstance(data, dict) or set(data) != {"roles", "matched_inputs_sha256"}
        or not re.fullmatch(r"[0-9a-f]{64}", data["matched_inputs_sha256"])):
        raise ValueError("missing checked compiler, configuration and workload binding")
    roles = data["roles"]
    if not isinstance(roles, dict) or set(roles) != set(_ROLES):
        raise ValueError("missing pinned build role")
    for product in roles.values():
        if (set(product) != {"path", "sha256", "revision", "build_receipt_sha256"}
            or not PureWindowsPath(product["path"]).is_absolute()
            or ".." in PureWindowsPath(product["path"]).parts
            or not re.fullmatch(r"[0-9a-f]{40}", product["revision"])
            or not all(re.fullmatch(r"[0-9a-f]{64}", product[k]) for k in ("sha256", "build_receipt_sha256"))):
            raise ValueError("invalid source, image or build receipt binding")
        product["path"] = str(PureWindowsPath(product["path"]))
    a, b = roles["control"], roles["reference"]
    if (a["revision"] != b["revision"] or a["build_receipt_sha256"] == b["build_receipt_sha256"]
        or PureWindowsPath(a["path"]) == PureWindowsPath(b["path"])):
        raise ValueError("controls require independent builds from the same source")
    candidate = roles["candidate"]
    for other in (a, b):
        if (PureWindowsPath(candidate["path"]) == PureWindowsPath(other["path"])
            and any(candidate[k] != other[k] for k in ("sha256", "revision", "build_receipt_sha256"))):
            raise ValueError("one image path has conflicting source or build bindings")
    return roles


def raw_hashes(directory):
    """Bind the complete small raw capture to its independently retained receipt."""
    from composition_runner import source_hashes
    return {"samples": source_hashes(directory / "samples"),
            "journal": evidence._sha256(directory / "telemetry/processes.jsonl"),
            "host": evidence._sha256(directory / "telemetry/telemetry.jsonl")}


def capture_bound(directory, roles, pin, witnessed):
    """Start under the Windows host-reserve guard, inside its capped parent job."""
    import composition_runner as runner
    from windows_process import JobExtendedLimits, WindowsProcessApi

    host = runner.windows_host()
    limits = JobExtendedLimits()
    WindowsProcessApi().checked("QueryInformationJobObject", None, 9, ctypes.byref(limits),
                                ctypes.sizeof(limits), None)
    flags = limits.BasicLimitInformation.LimitFlags
    if flags & 0x2200 != 0x2200 or not 0 < limits.JobMemoryLimit <= _JOB_BYTES:
        raise ValueError("use the Windows memory guard with a two-GiB enclosing job")
    if shutil.disk_usage(directory.parent).free <= 20 * 1024**3:
        raise ValueError("insufficient disk reserve on the output volume")
    with runner.machine_lock(Path(tempfile.gettempdir()) / runner.LOCK.name):
        directory.mkdir(exist_ok=False)
        monitor = runner.WindowsMonitor(directory / "telemetry", evidence.load_protocol(runner.WINDOWS_PROTOCOL))
        images = {role: (Path(p["path"]), p["sha256"]) for role, p in roles.items()}
        result = capture_screen(monitor, images, directory, directory / "samples",
                                dict(os.environ), witnessed_candidate=witnessed)
        runner.check_host_unchanged(host, monitor)
        runner.write_json(directory / "capture.json", {"directory": str(directory), "host": host,
                          "bindings_sha256": pin, "witnessed_candidate": witnessed,
                          "raw_sha256": raw_hashes(directory), "result": result})
    return {**result, "capture_sha256": evidence._sha256(directory / "capture.json")}


def main():
    parser = argparse.ArgumentParser(description="Measure or replay a PR screen, never a release verdict.")
    parser.add_argument("mode", choices=("capture", "replay"))
    parser.add_argument("directory", type=Path)
    parser.add_argument("--builds", type=Path, required=True, help="reviewed build bindings JSON")
    parser.add_argument("--builds-sha256", required=True, help="independent digest of the reviewed bindings")
    parser.add_argument("--capture-sha256", help="replay only: independently retained digest from capture")
    parser.add_argument("--witnessed-candidate", action="store_true",
                        help="require the injected fault's milestones at calls 1, 256 and 1,024")
    args = parser.parse_args()
    roles = build_bindings(args.builds, args.builds_sha256)
    directory = args.directory.resolve()
    if args.mode == "capture":
        if args.capture_sha256 is not None:
            raise ValueError("cannot supply a capture pin before collection")
        result = capture_bound(directory, roles, args.builds_sha256, args.witnessed_candidate)
    else:
        import hashlib
        raw = (directory / "capture.json").read_bytes()
        if hashlib.sha256(raw).hexdigest() != args.capture_sha256:
            raise ValueError("replay requires the independently retained capture pin")
        capture_record = json.loads(raw)
        host_events = [json.loads(line) for line in (directory / "telemetry/telemetry.jsonl").read_bytes().splitlines()]
        if (capture_record["raw_sha256"] != raw_hashes(directory) or len(host_events) != 1
            or host_events[0].get("phase") != "final-host" or host_events[0].get("host") != capture_record["host"]):
            raise ValueError("changed raw capture or missing final host identity")
        if (capture_record["bindings_sha256"] != args.builds_sha256
            or capture_record["result"]["images"] !=
                {role: {"path": p["path"], "sha256": p["sha256"]} for role, p in roles.items()}
            or capture_record["witnessed_candidate"] is not args.witnessed_candidate):
            raise ValueError("capture inputs differ from requested replay")
        cwd = PureWindowsPath(capture_record["directory"])
        result = replay_screen(directory / "samples", directory / "telemetry/processes.jsonl", cwd=cwd,
                               destination=cwd / "samples", images={r: p["path"] for r, p in roles.items()},
                               witnessed_candidate=args.witnessed_candidate)
        if any(capture_record["result"].get(k) != v for k, v in result.items() if k != "process_seconds"):
            raise ValueError("replayed measurements differ from the capture")
        result["capture_sha256"] = args.capture_sha256
    print(json.dumps(result, allow_nan=False))
    return {"pass": 0, "fail": 1, "inconclusive": 2}[result["verdict"]]


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f"invalid cancellation evidence: {error}", file=sys.stderr)
        raise SystemExit(3) from error
