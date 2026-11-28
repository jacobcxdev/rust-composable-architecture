"""Measure the enrolled reference A allocation and debug-zero hosts, never a candidate verdict."""
import hashlib
import json
from pathlib import Path

import composition_performance as evidence
from composition_registry_seed import _regular, _sha
import composition_runner as runner
import composition_windows_absolute as absolute
import composition_windows_complete as complete
from qualification_reference_cache import verify_reference_products


def capture(cache, *, enrolment_sha256, inputs, raw, cwd, env, monitor):
    """Use the fixed allocation budgets and require fresh processes for all 47+2 workloads."""
    raw, cwd, cache = map(Path, (raw, cwd, cache))
    if (not monitor.complete or not monitor.experimental_rank or raw.resolve() != raw
            or cwd.resolve(strict=True) != cwd or not cwd.is_dir() or raw.is_relative_to(cache)
            or cwd.is_relative_to(cache) or any(k.upper().startswith("RCA_COMPOSITION_ALLOCATION_") for k in env)):
        raise ValueError("expected guarded reference jobs, clean workload settings and outputs outside the cache")
    options = dict(enrolment_sha256=enrolment_sha256, inputs=inputs, purpose="composition-allocations",
                   roles=("a",), names=("allocation", "zero"))
    products = verify_reference_products(cache, **options)["a"]
    enrolled = _regular(cache / "enrolment.json").read_bytes()
    if hashlib.sha256(enrolled).hexdigest() != enrolment_sha256:
        raise ValueError("reference enrolment changed during admission")
    images = json.loads(enrolled)["roles"]["a"]["products"]
    if products["allocation"].samefile(products["zero"]):
        raise ValueError("allocation and debug-zero hosts must be separate products")
    manifest = evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    budget = evidence.validate_budget_document(runner.BUDGET)
    policy = complete.load_policy(runner.WINDOWS_COMPLETE_POLICY)
    raw.mkdir()
    sealed = {}
    last_exit = 0

    def run(product, args, log, settings):
        nonlocal last_exit
        if _sha(_regular(products[product])) != images[product]:
            raise ValueError("reference image changed before measurement")
        monitor.run([str(products[product]), *args], log, cwd, settings)
        resources = _regular(log.with_suffix(".resources.json"))
        fact = json.loads(resources.read_bytes())
        evidence.validate_windows_resource_counters(fact, expected_kind="runtime")
        counters = fact["direct_process"]
        if not last_exit < counters["CreationTime100ns"] < counters["ExitTime100ns"]:
            raise ValueError("overlapping or repeated reference process lifetime")
        last_exit = counters["ExitTime100ns"]
        for p in (log, resources):
            sealed[p.relative_to(raw).as_posix()] = _sha(_regular(p))

    lines = []
    for name, workload in manifest.items():
        log = raw / "allocation" / (name + ".txt")
        run("allocation", [], log, {**env, "RCA_COMPOSITION_ALLOCATION_SCENARIO": name,
                                    "RCA_COMPOSITION_ALLOCATION_SAMPLES": str(workload["samples"])})
        result = log.read_text(encoding="utf-8").splitlines()
        if len(result) != 1 or not result[0].startswith(f"scenario={name} "):
            raise ValueError(f"reference process did not report its selected workload: {name}")
        lines.append(result[0])
    aggregate = raw / "allocations.txt"
    with aggregate.open("x", encoding="utf-8") as out:
        out.write("\n".join(lines) + "\n")
    sealed[aggregate.name] = _sha(aggregate)
    rows = absolute.allocation_observations(aggregate)
    zero = {}
    for label, name in absolute.ZERO_TESTS.items():
        log = raw / f"zero-{label}.txt"
        run("zero", [name, "--exact", "--nocapture"], log, env)
        zero[label] = absolute._zero_test(log, name)

    selected = absolute.selected_allocation_values(rows, {label: True for label in zero})
    gates = [r for r in budget["rows"] if r["scenario"].startswith("allocation/")]
    values = {(r["scenario"], r["measured_statistic"]): selected[r["scenario"]] for r in gates}
    metrics = [complete._result(r, values) for r in complete._reference_rows(policy, gates, values)]
    verify_reference_products(cache, **options)
    if any(_sha(_regular(raw / p)) != digest for p, digest in sealed.items()):
        raise ValueError("reference process evidence changed during capture")
    observation = {"scope": "windows-reference-a-allocations", "enrolment_sha256": enrolment_sha256,
                   "images_sha256": images, "raw_sha256": sealed, "allocations": rows, "zero_tests": zero,
                   "selected": selected, "metrics": metrics}
    runner.write_json(raw / "result.json", observation)
    return observation
