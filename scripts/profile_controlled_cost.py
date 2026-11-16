"""Compare controlled instruction profiles without averaging unexplained drift.

This tool compares identified executables, not inferred source revisions. Keep
their build receipts with its output. It does not qualify native timing or prove
that fewer instructions produce lower latency on a physical processor.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


# One fixture's independently specified actor polls, client polls, commits,
# arithmetic checksum and decimal action-order checksum.
SCHEDULES = {
    "admission-empty-ab": (3, 9, 2, 3, 12),
    "admission-empty-ba": (3, 9, 2, 3, 21),
    "admission-full-ab": (4, 10, 2, 3, 12),
    "admission-full-ba": (4, 10, 2, 3, 21),
    "graceful-idle": (2, 3, 0, 0, 0),
    "graceful-queued": (2, 9, 2, 3, 12),
    "abortive-unpolled": (1, 1, 0, 0, 0),
    "abortive-idle": (2, 1, 0, 0, 0),
    "abortive-query": (3, 3, 0, 0, 0),
}
ITERATIONS = 16
REPETITIONS = 3
CHILD_ENVIRONMENT = {"PATH": "/usr/bin:/bin", "LANG": "C", "LC_ALL": "C", "TZ": "UTC"}
CHILD_DIRECTORY = "/"


def parse_annotation(text):
    """Require a complete, exclusive instruction-count table from Callgrind."""
    if re.findall(r"^Events shown:\s*(.*?)\s*$", text, re.MULTILINE) != ["Ir"]:
        raise ValueError("annotation must show only Ir")
    totals = []
    functions = {}
    for line in text.splitlines():
        match = re.fullmatch(r"\s*([0-9][0-9,]*)\s+(.+?)\s*", line)
        if not match:
            continue
        count = int(match[1].replace(",", ""))
        name = match[2]
        if name == "PROGRAM TOTALS":
            totals.append(count)
        elif name in functions:
            raise ValueError("duplicate function in annotation")
        elif count:
            functions[name] = count
    if len(totals) != 1 or totals[0] <= 0 or sum(functions.values()) != totals[0]:
        raise ValueError("missing, zero or incomplete instruction totals")
    return {"instructions": totals[0], "functions": functions}


def validate_record(record):
    """Reject a successful-looking record with the wrong work or ownership."""
    if not isinstance(record, dict) or record.get("scenario") not in SCHEDULES:
        raise ValueError("unknown controlled work record")
    iterations = record.get("iterations")
    sensitivity = record.get("sensitivity")
    if type(iterations) is not int or not 1 <= iterations <= 256:
        raise ValueError("invalid iteration count")
    if type(sensitivity) is not int or sensitivity not in (0, 64):
        raise ValueError("invalid sensitivity count")
    expected = {
        "schema": "rca-controlled-cost-v1", "runtime": "manual",
        "boundary": "profile_controlled_work", "payload_bytes": 65536,
        "driver_empty": True, "owners_released": True, "runtime_abandonments": 0,
        "state_drops": iterations, "reducer_drops": iterations,
        "actor_drops": iterations, "spawns": iterations,
        "sensitivity_checksum": 2080 * iterations if sensitivity else 0,
    }
    expected.update(zip(
        ("actor_polls", "client_polls", "commits", "checksum", "order_checksum"),
        (count * iterations for count in SCHEDULES[record["scenario"]]), strict=True,
    ))
    for key, value in expected.items():
        if type(record.get(key)) is not type(value) or record[key] != value:
            raise ValueError(f"invalid work record field: {key}")


def validate_profile(profile):
    """Check counted work and the intended entry boundary before comparison."""
    total, functions = profile.get("instructions"), profile.get("functions")
    if type(total) is not int or total <= 0 or not isinstance(functions, dict):
        raise ValueError("invalid instruction profile")
    if not all(isinstance(key, str) and type(value) is int and value > 0
               for key, value in functions.items()):
        raise ValueError("invalid function counts")
    if sum(functions.values()) != total:
        raise ValueError("incomplete function counts")
    if not any("profile_controlled_work" in name for name in functions):
        raise ValueError("controlled work boundary was not captured")


def verify_repetitions(samples):
    """Require exact equality, including attribution, across all three runs."""
    if len(samples) != REPETITIONS:
        raise ValueError("exactly three repetitions are required")
    for sample in samples:
        validate_record(sample["record"])
        validate_profile(sample["profile"])
        if sample != samples[0]:
            raise ValueError("repeatability drift: records or function counts differ")
    return samples[0]


def semantic_record(record):
    """Exclude only the two explicitly separate arithmetic-control fields."""
    return {key: value for key, value in record.items()
            if key not in ("sensitivity", "sensitivity_checksum")}


def verify_sensitivity(ordinary, sensitive):
    """Require the added arithmetic to appear in its named function."""
    for sample in (ordinary, sensitive):
        validate_record(sample["record"])
        validate_profile(sample["profile"])
    if ordinary["record"]["sensitivity"] != 0 or sensitive["record"]["sensitivity"] != 64:
        raise ValueError("wrong sensitivity inputs")
    if semantic_record(ordinary["record"]) != semantic_record(sensitive["record"]):
        raise ValueError("sensitivity changed useful Store work")
    difference = sensitive["profile"]["instructions"] - ordinary["profile"]["instructions"]
    attributed = [sum(count for name, count in sample["profile"]["functions"].items()
                      if "profile_extra_work" in name) for sample in (ordinary, sensitive)]
    if difference <= 0 or attributed[1] <= attributed[0]:
        raise ValueError("added work was not detected at its intended function")
    return difference


def compare(reference, candidate):
    """Report increases without interpreting repeatability as a performance pass."""
    for sample in (reference, candidate):
        validate_record(sample["record"])
        validate_profile(sample["profile"])
    if reference["record"] != candidate["record"]:
        raise ValueError("products performed different controlled work")
    before, after = (sample["profile"]["instructions"] for sample in (reference, candidate))
    difference = after - before
    return {
        "reference_instructions": before, "candidate_instructions": after,
        "difference": difference, "ratio": after / before,
        "direction": "increased" if difference > 0 else "decreased" if difference < 0 else "unchanged",
    }


def digest(path):
    """Identify a file without retaining its bytes in memory."""
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def collect(reference, candidate, output):
    """Collect one finite matrix; stop on the first unexplained discrepancy."""
    if sys.platform != "linux":
        raise ValueError("controlled instruction capture requires Linux and Callgrind")
    tools = {name: Path(shutil.which(name) or "") for name in ("valgrind", "callgrind_annotate")}
    if not all(path.is_file() for path in tools.values()):
        raise ValueError("valgrind and callgrind_annotate are required")
    if any(os.environ.get(name) for name in ("VALGRIND_LIB", "LD_PRELOAD", "LD_LIBRARY_PATH")):
        raise ValueError("ambient profiler or library overrides are unsupported")
    engines = [tools["valgrind"].resolve().parent.parent / directory / "valgrind/callgrind-amd64-linux"
               for directory in ("libexec", "lib")]
    engine = next((path for path in engines if path.is_file()), None)
    if engine is None:
        raise ValueError("the x86-64 Callgrind engine must be identifiable")
    tools["callgrind_engine"] = engine
    binaries = {"reference": Path(reference).resolve(), "candidate": Path(candidate).resolve()}
    hashes = {role: digest(path) for role, path in binaries.items()}
    tool_hashes = {name: digest(path) for name, path in tools.items()}
    if shutil.disk_usage(output.parent).free < 20 * 1024**3:
        raise ValueError("instruction capture requires twenty GiB of disk reserve")
    output.mkdir(exist_ok=False)
    result = {
        "schema": "rca-controlled-instruction-comparison-v1", "complete": False,
        "collector_sha256": digest(__file__), "python": sys.version,
        "scope": list(SCHEDULES), "iterations": ITERATIONS, "repetitions": REPETITIONS,
        "native_timing_qualified": False, "source_revision_inferred": False,
        "child_environment": CHILD_ENVIRONMENT, "child_directory": CHILD_DIRECTORY,
        "binaries": {role: {"path": str(path), "sha256": hashes[role]} for role, path in binaries.items()},
        "tools": {name: {"path": str(path), "sha256": tool_hashes[name]} for name, path in tools.items()},
        "commands": [], "observations": [], "comparisons": [],
    }

    def run(command, destination):
        timeout = None
        try:
            process = subprocess.run(
                list(map(str, command)), capture_output=True, text=True, timeout=120,
                env=CHILD_ENVIRONMENT, cwd=CHILD_DIRECTORY,
            )
            stdout, stderr, returncode = process.stdout, process.stderr, process.returncode
        except subprocess.TimeoutExpired as error:
            timeout = error
            stdout, stderr, returncode = error.stdout, error.stderr, None
        for path, contents in ((destination, stdout), (destination.with_suffix(destination.suffix + ".stderr"), stderr)):
            data = contents if isinstance(contents, bytes) else (contents or "").encode()
            path.write_bytes(data)
        result["commands"].append({
            "argv": list(map(str, command)), "returncode": returncode, "timed_out": timeout is not None,
            "stdout": destination.name, "stdout_sha256": digest(destination),
            "stderr_sha256": digest(destination.with_suffix(destination.suffix + ".stderr")),
        })
        if timeout is not None:
            raise timeout
        if returncode:
            raise ValueError(f"profile command failed: {destination.name}")
        return stdout

    try:
        result["valgrind_version"] = run([tools["valgrind"], "--version"], output / "version.txt").strip()
        for scenario in SCHEDULES:
            groups = {(role, sensitivity): [] for role in binaries for sensitivity in (0, 64)}
            for sensitivity in (0, 64):
                for repetition in range(REPETITIONS):
                    roles = ("reference", "candidate") if repetition % 2 == 0 else ("candidate", "reference")
                    for role in roles:
                        if digest(binaries[role]) != hashes[role]:
                            raise ValueError("measured executable changed")
                        label = f"{scenario}-{role}-{sensitivity}-{repetition}"
                        raw = output / (label + ".callgrind")
                        stdout = run([
                            tools["valgrind"], "--command-line-only=yes", "--tool=callgrind", "--cache-sim=no", "--branch-sim=no",
                            "--collect-atstart=no", "--toggle-collect=*profile_controlled_work*",
                            "--callgrind-out-file=" + str(raw), binaries[role], scenario, str(ITERATIONS), str(sensitivity),
                        ], output / (label + ".json"))
                        record = json.loads(stdout)
                        validate_record(record)
                        requested = scenario, ITERATIONS, sensitivity
                        if (record["scenario"], record["iterations"], record["sensitivity"]) != requested:
                            raise ValueError("returned work does not match the requested scenario and dimensions")
                        annotation = run([
                            tools["callgrind_annotate"], "--show=Ir", "--show-percs=no", "--auto=no",
                            "--inclusive=no", "--threshold=100", raw,
                        ], output / (label + ".txt"))
                        observation = {"record": record, "profile": parse_annotation(annotation)}
                        groups[role, sensitivity].append(observation)
                        result["observations"].append({"label": label, "role": role,
                                                       "profile_sha256": digest(raw), **observation})
            verified = {key: verify_repetitions(values) for key, values in groups.items()}
            sensitivity = {role: verify_sensitivity(verified[role, 0], verified[role, 64]) for role in binaries}
            comparison = compare(verified["reference", 0], verified["candidate", 0])
            result["comparisons"].append({"scenario": scenario, "sensitivity_deltas": sensitivity, **comparison})
            print(json.dumps({"scenario": scenario, "repeatable": True, **comparison}), flush=True)
        if any(digest(path) != hashes[role] for role, path in binaries.items()):
            raise ValueError("measured executable changed")
        if any(digest(path) != tool_hashes[name] for name, path in tools.items()):
            raise ValueError("profiling tools changed")
        result["complete"] = True
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        result["error"] = str(error)
        raise
    finally:
        (output / "comparison.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    """Require explicit products and a fresh evidence directory."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    try:
        collect(args.reference, args.candidate, args.output.resolve())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, str(error) + "\n")


if __name__ == "__main__":
    main()
