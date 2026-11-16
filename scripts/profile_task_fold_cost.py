"""Compare fixed-clock Task and fold instruction work, not native speed."""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

from profile_controlled_cost import CHILD_ENVIRONMENT, digest, parse_annotation


ROWS = [("task-named-shared", 1, 16)]
for carrier in ("shared", "local"):
    ROWS.extend((f"task-{kind}-{carrier}", 1, 16) for kind in ("unnamed", "named")
                if f"task-{kind}-{carrier}" != "task-named-shared")
    for scope in ("root", "subtree"):
        ROWS.extend((f"fold-{scope}{suffix}-{carrier}", size, iterations)
                    for suffix, size, iterations in
                    (("-empty", 1, 16), ("", 3, 16), ("", 256, 16),
                     ("-sparse", 1024, 16), ("-churn", 16, 3)))


def validate_record(stdout, row):
    """Check the literal workload oracle and discard only unqualified wall time."""
    lines = stdout.splitlines()
    if len(lines) != 3 or lines[:2] != ["READY", "DONE"]:
        raise ValueError("invalid controller protocol")
    record = json.loads(lines[2])
    name, size, iterations = row
    contribution = 2 if "subtree" in name else 6
    total = contribution * (64 if "sparse" in name else size)
    checksum = 1 if name.startswith("task-") else 0 if "empty" in name else total
    if "churn" in name:
        checksum = 2 * total - 2
    expected = {
        "schema": "rca-task-fold-controlled-cost-v1", "workload": name,
        "size": size, "iterations": iterations, "workers": 1,
        "operations": iterations * (4 if "churn" in name else 1),
        "checksum": iterations * checksum, "latencies_ns": [], "cleanup_complete": True,
        "details": {"allocator": "system", "runtime": "Tokio current-thread",
                    "task_groups_enabled": False, "clock": "paused-checked",
                    "boundary": "profile_task_fold_work",
                    "scope": "construction, checked work, shutdown and host disposal"},
    }
    if not isinstance(record, dict) or type(record.get("elapsed_ns")) is not int or record["elapsed_ns"] <= 0:
        raise ValueError("invalid diagnostic elapsed time")
    semantic = {key: value for key, value in record.items() if key != "elapsed_ns"}
    # JSON distinguishes bool from int, unlike Python's ordinary equality.
    if json.dumps(semantic, sort_keys=True) != json.dumps(expected, sort_keys=True):
        raise ValueError("wrong controlled schema, clock, work or cleanup")
    return semantic


def verify_repetitions(samples):
    """Require all three complete function maps to agree, not just their totals."""
    if len(samples) != 3 or any(sample != samples[0] for sample in samples[1:]):
        raise ValueError("repeatability drift in work or full function counts")
    return samples[0]


def verify_artifact(artifact, head, baseline):
    """Bind trusted source and exact build metadata before executing any binary."""
    if (artifact / "candidate-source-head.txt").read_text().strip() != head:
        raise ValueError("artifact source head differs from requested source")
    identity = (artifact / "candidate-git.txt").read_text().splitlines()
    if len(identity) != 2 or any(not re.fullmatch("[0-9a-f]{40}", item) for item in identity):
        raise ValueError("artifact build identity is invalid")
    if (artifact / "baseline-git.txt").read_text().splitlines()[0] != baseline:
        raise ValueError("artifact baseline differs from requested baseline")
    if "All selected checks passed" not in (artifact / "complete.txt").read_text():
        raise ValueError("artifact build did not complete")
    if "PROFILE_TARGET=x86_64-unknown-linux-gnu" not in (artifact / "tools.txt").read_text():
        raise ValueError("artifact target is not Linux x86-64")
    if not (artifact / "harness.sha256").read_text().strip():
        raise ValueError("artifact harness identities are missing")
    hashes = {}
    for line in (artifact / "binaries.sha256").read_text().splitlines():
        checksum, name = line.split("  ", 1)
        if not re.fullmatch(r"(?:baseline|candidate)-task-fold(?:-controlled)?(?:-groups)?", name):
            raise ValueError("unexpected artifact executable")
        if name in hashes or digest(artifact / name) != checksum:
            raise ValueError("artifact binary hash mismatch")
        hashes[name] = checksum
    binaries = {role: artifact / f"{role}-task-fold-controlled" for role in ("baseline", "candidate")}
    if any(path.name not in hashes for path in binaries.values()):
        raise ValueError("controlled binaries missing")
    return binaries


def collect(artifact, head, baseline, output):
    """Stop at the first unstable row and retain every completed or failed run."""
    if sys.platform != "linux" or any(os.environ.get(key) for key in ("VALGRIND_LIB", "LD_PRELOAD", "LD_LIBRARY_PATH")):
        raise ValueError("Linux without profiler or loader overrides is required")
    binaries = verify_artifact(artifact, head, baseline)
    tools = {name: shutil.which(name) for name in ("valgrind", "callgrind_annotate")}
    if not all(tools.values()):
        raise ValueError("Valgrind tools are missing")
    engine = next((path for path in (Path("/usr/libexec/valgrind/callgrind-amd64-linux"),
                                    Path("/usr/lib/valgrind/callgrind-amd64-linux")) if path.is_file()), None)
    if engine is None:
        raise ValueError("the actual Callgrind engine must be identifiable")
    if shutil.disk_usage(output.parent).free < 4 * 1024**3:
        raise ValueError("instruction capture requires four GiB of disk headroom")
    output.mkdir(exist_ok=False)
    result = {"schema": "rca-task-fold-instructions-v1", "complete": False,
              "native_timing_qualified": False, "rows": ROWS, "repetitions": 3,
              "collector_sha256": digest(__file__), "parser_sha256": digest(Path(__file__).with_name("profile_controlled_cost.py")),
              "source_head": head, "baseline_head": baseline,
              "harness_identity_sha256": digest(artifact / "harness.sha256"),
              "binaries": {role: digest(path) for role, path in binaries.items()},
              "tools": {name: {"path": path, "sha256": digest(path)} for name, path in tools.items()},
              "engine": {"path": str(engine), "sha256": digest(engine)},
              "commands": [], "observations": [], "comparisons": []}

    def run(command, label, stdin=None):
        try:
            completed = subprocess.run(command, input=stdin, capture_output=True, text=True,
                                       timeout=120, env=CHILD_ENVIRONMENT, cwd="/")
            stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
        except subprocess.TimeoutExpired as error:
            stdout, stderr, code = error.stdout or b"", error.stderr or b"", None
        for suffix, data in (("stdout", stdout), ("stderr", stderr)):
            (output / f"{label}.{suffix}").write_bytes(data.encode() if isinstance(data, str) else data)
        result["commands"].append({"argv": command, "returncode": code, "label": label})
        if code != 0:
            raise ValueError(f"command failed or timed out: {label}")
        return stdout

    try:
        result["valgrind_version"] = run([tools["valgrind"], "--version"], "version").strip()
        for row in ROWS:
            if shutil.disk_usage(output).free < 4 * 1024**3:
                raise ValueError("instruction capture reached its disk reserve")
            groups = {role: [] for role in binaries}
            name, size, iterations = row
            for repetition in range(3):
                for role in (("baseline", "candidate") if repetition % 2 == 0 else ("candidate", "baseline")):
                    label = f"{name}-{size}-{role}-{repetition}"
                    raw = output / f"{label}.callgrind"
                    stdout = run([tools["valgrind"], "--command-line-only=yes", "--tool=callgrind",
                                  "--cache-sim=no", "--branch-sim=no", "--collect-atstart=no",
                                  "--toggle-collect=*profile_task_fold_work*", f"--callgrind-out-file={raw}",
                                  str(binaries[role]), name, str(size), str(iterations), "1"], label,
                                 "GO\nREPORT\nEXIT\n")
                    record = validate_record(stdout, row)
                    annotation = run([tools["callgrind_annotate"], "--show=Ir", "--show-percs=no",
                                      "--auto=no", "--inclusive=no", "--threshold=100", str(raw)], label + "-annotation")
                    profile = parse_annotation(annotation)
                    if not any("profile_task_fold_work" in name for name in profile["functions"]):
                        raise ValueError("intended instruction boundary is missing")
                    sample = {"record": record, "profile": profile}
                    groups[role].append(sample)
                    result["observations"].append({"label": label, **sample})
            samples = {role: verify_repetitions(group) for role, group in groups.items()}
            if samples["baseline"]["record"] != samples["candidate"]["record"]:
                raise ValueError("baseline and candidate performed different work")
            before, after = (samples[role]["profile"]["instructions"] for role in ("baseline", "candidate"))
            result["comparisons"].append({"row": row, "baseline": before, "candidate": after, "ratio": after / before})
            print(json.dumps(result["comparisons"][-1]), flush=True)
        if any(digest(path) != result["binaries"][role] for role, path in binaries.items()):
            raise ValueError("measured executable changed")
        if any(digest(path) != result["tools"][name]["sha256"] for name, path in tools.items()) or digest(engine) != result["engine"]["sha256"]:
            raise ValueError("profiling tools changed")
        result["complete"] = True
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
        raise
    finally:
        (output / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifact", type=Path)
    parser.add_argument("head")
    parser.add_argument("baseline")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    collect(args.artifact.resolve(), args.head, args.baseline, args.output.resolve())
