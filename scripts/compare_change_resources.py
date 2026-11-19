"""Compare work-phase allocation counts, not native timing or release performance."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
from typing import NamedTuple


class Case(NamedTuple):
    """Arguments and observable work required from the existing profile binary."""

    args: tuple[str, ...]
    expected: dict


POOL = Case(("mixed-pool-shared-ordered", "1000", "1"), {
    "schema": "rca-performance-profile-v2", "backend": "deterministic",
    "domain": "Shared-Arc", "width": 1000, "capacity": 1, "iterations": 1, "sum": 20,
    "useful_work": {"accepted_commands": 4, "full_rejections": 1, "pending_outputs": 1,
                    "outputs": 2, "captured_tasks": 2, "captured_completions": 1,
                    "captured_cancellations": 1, "captured_waits_finished": 2,
                    "later_tasks_pending": 2, "later_tasks_cancelled": 2,
                    "structural_commits": 2, "break_callbacks": 1, "full_callbacks": 1000},
    "ownership": {"state_created": 1001, "state_dropped": 1001,
                  "task_created": 4, "task_dropped": 4, "alive": 0},
})
CASES = {
    "dispatch": Case(("topology-deterministic", "1", "0", "64"), {
        "schema": "rca-feature-topology-profile-v1", "width": 0, "depth": 1,
        "backend": "deterministic", "empty_root_commits": 64,
        "empty_root_causal_completed": 64, "iterations": 64, "owners_released": True,
    }),
    "keyed-routing": Case(("keyed-steady-deterministic", "2", "1"), {
        "schema": "rca-keyed-performance-profile-v1", "size": 2, "iterations": 1,
        "roots_committed": 3, "responses_committed": 2, "leaf_reductions": 4,
        "owners_released": True,
    }),
    "cancellation-shutdown": Case(("keyed-retire-deterministic", "2", "1"), {
        "schema": "rca-keyed-performance-profile-v1", "size": 2, "iterations": 1,
        "roots_committed": 5, "tasks_retired": 1, "tasks_completed": 1,
        "owners_released": True,
    }),
    "queue-pressure": POOL,
    "headless-worker": POOL,
}
METRICS = ("allocated_blocks", "allocated_bytes")


def select(families):
    """Return one command per workload even when two families share it."""
    selected = []
    for family in families.split(","):
        if family not in CASES:
            raise ValueError(f"no runtime allocation workload for {family!r}")
        if CASES[family] not in selected:
            selected.append(CASES[family])
    return selected


def observation(case, result):
    """Reject changed useful work, unmeasured fields and retained allocations."""
    if not isinstance(result, dict):
        raise ValueError("expected one profile record")
    facts = {key: value for key, value in result.items() if key != "allocations"}
    for key, value in {**case.expected, "workload": case.args[0]}.items():
        if json.dumps(facts.get(key), sort_keys=True) != json.dumps(value, sort_keys=True):
            raise ValueError(f"{case.args[0]}: unexpected {key}")
    allocations = result.get("allocations")
    if not isinstance(allocations, dict) or allocations.get("instrument") != "dhat-0.3.3":
        raise ValueError("missing pinned DHAT observation")
    retained = allocations.get("executor_thread_local")
    if not isinstance(retained, dict) or any(
        type(retained.get(k)) is not int or retained[k] != 0
        for k in ("retained_blocks", "retained_bytes")
    ):
        raise ValueError("retention is nonzero or unmeasured")
    phases = allocations.get("phases")
    work = phases.get("work") if isinstance(phases, dict) else None
    if not isinstance(work, dict) or any(
        type(work.get(k)) is not int or work[k] < 0 for k in METRICS
    ):
        raise ValueError("missing or invalid work allocation counts")
    return json.dumps(facts, sort_keys=True, separators=(",", ":")), {k: work[k] for k in METRICS}


def run(binary, args):
    """Run one bounded process without preserving optional full heap traces."""
    env = os.environ.copy()
    env.pop("RCA_DHAT_PROFILE", None)
    with tempfile.TemporaryDirectory(prefix="rca-work-allocation-") as cwd:
        process = subprocess.run([binary, *args], cwd=cwd, env=env, timeout=30,
                                 capture_output=True, text=True)
    if process.returncode:
        raise ValueError(f"{args[0]}: exit {process.returncode}: {process.stderr[-500:]}")
    return json.loads(process.stdout)


def compare_pool(baseline, candidate, collect):
    """Screen independently launched processes; these are not independent builds."""
    pairs = []
    facts = None
    adverse = {key: 0 for key in METRICS}
    control_outside_limit = False
    for block in range(14):
        subject_first = block % 2 == 1
        arms = ("comparison", "control") if block // 2 % 2 == 0 else ("control", "comparison")
        for arm in arms:
            subject = candidate if arm == "comparison" else baseline
            images = (subject, baseline) if subject_first else (baseline, subject)
            counts = []
            for image in images:
                observed, metrics = observation(POOL, collect(image, POOL.args))
                if facts is not None and observed != facts:
                    raise ValueError("worker pool useful work differs")
                facts = observed
                counts.append(metrics)
            base, measured = reversed(counts) if subject_first else counts
            pairs.append({"arm": arm, "order": "subject-first" if subject_first else "baseline-first",
                          "baseline": base, "subject": measured})
            for key in METRICS:
                exceeds = 10 * measured[key] > 11 * base[key]
                if arm == "comparison":
                    adverse[key] += exceeds
                else:
                    # Neither unchanged process may cost over 10% more than its peer.
                    control_outside_limit |= exceeds or 10 * base[key] > 11 * measured[key]
    regressions = [key for key, count in adverse.items() if count == 14]
    if control_outside_limit:
        decision = "inconclusive"
    elif regressions:
        decision = "regression"
    elif any(adverse.values()):
        decision = "inconclusive"
    else:
        decision = "pass"
    return {
        "args": POOL.args, "method": "fixed-paired-process-screen",
        "control": "separate processes from the same baseline binary",
        "work_sha256": hashlib.sha256(facts.encode()).hexdigest(),
        "pairs": pairs, "adverse_comparison_pairs": adverse,
        "control_outside_limit": control_outside_limit,
        "exceeds_limit": regressions if not control_outside_limit else [],
        "decision": decision,
    }


def compare_case(case, baseline, candidate, collect=run):
    """Compare identical useful work under the rule fixed for this workload."""
    if case == POOL:
        return compare_pool(baseline, candidate, collect)
    samples = [
        observation(case, collect(image, case.args))
        for image in (baseline, candidate, candidate, baseline)
    ]
    if len({facts for facts, _ in samples}) != 1:
        raise ValueError(f"{case.args[0]}: useful work differs")
    (facts, before), (_, after), (_, after_repeat), (_, before_repeat) = samples
    row = {"args": case.args, "method": "exact-repeat",
           "order": ["baseline", "candidate", "candidate", "baseline"],
           "work_sha256": hashlib.sha256(facts.encode()).hexdigest(),
           "baseline": before, "candidate": after}
    if before != before_repeat or after != after_repeat:
        return {**row, "decision": "inconclusive", "error": "allocation counts did not repeat",
                "baseline_repeat": before_repeat, "candidate_repeat": after_repeat}
    adverse = [key for key in METRICS if 10 * after[key] > 11 * before[key]]
    return {**row, "exceeds_limit": adverse, "decision": "regression" if adverse else "pass"}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-bin", type=Path, required=True)
    parser.add_argument("--candidate-bin", type=Path, required=True)
    parser.add_argument("--families", required=True)
    args = parser.parse_args(argv)
    start = time.monotonic()
    report = {"scope": "work-phase-allocations-only", "families": args.families, "rows": []}
    try:
        cases = select(args.families)
        images = [p.resolve(strict=True) for p in (args.baseline_bin, args.candidate_bin)]
        if images[0].samefile(images[1]):
            raise ValueError("two separately built binary paths are required")
        report["images"] = {}
        for role, image in zip(("baseline", "candidate"), images, strict=True):
            with image.open("rb") as source:
                report["images"][role] = hashlib.file_digest(source, "sha256").hexdigest()
        for case in cases:
            try:
                row = compare_case(case, *images)
            except (OSError, ValueError, subprocess.TimeoutExpired) as error:
                row = {"args": case.args, "decision": "inconclusive", "error": str(error)}
            report["rows"].append(row)
        decisions = {row["decision"] for row in report["rows"]}
        report["decision"] = next(d for d in ("regression", "inconclusive", "pass") if d in decisions)
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        report.update(decision="inconclusive", error=str(error))
    report["comparison_seconds"] = round(time.monotonic() - start, 6)
    print(json.dumps(report, sort_keys=True))
    return {"pass": 0, "regression": 1, "inconclusive": 2}[report["decision"]]


if __name__ == "__main__":
    raise SystemExit(main())
