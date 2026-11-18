"""Observe removal of a child with 10,000 ready tasks on Windows.

The sample count applies to complete fixtures, not individual tasks. The wall
window includes setup and cleanup. The reported median times the removal only.
The caller supplies the verified benchmark build and compares independent runs.
No result from this module qualifies a release on its own.
"""

import json
from pathlib import Path, PureWindowsPath
import re

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


def replay(transcript, events, *, executable, cwd, destination):
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

    median = evidence.validate_isolated_transcript(transcript, SCENARIO)[SCENARIO]
    # Read the count from the same Divan leaf, never from a summary or another line.
    leaves = [line for line in transcript.read_text(encoding="utf-8").splitlines()
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


def capture(monitor, executable, cwd, transcript, env, *, executable_sha256):
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
    return replay(transcript, events, executable=executable, cwd=cwd, destination=transcript)
