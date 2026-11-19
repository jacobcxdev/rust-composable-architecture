"""Measure two cold Cargo graphs and the first check of each unbuilt fixture.

One observation is not a pass. The caller owns sample selection, signed source
and host checks, independent controls, and the comparison with release limits.
"""

import argparse
import ctypes
import hashlib
import io
import json
import math
import os
from pathlib import Path, PureWindowsPath
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time

import composition_performance as evidence
import composition_registry_seed as registry


LEAVES = ("direct", "composed-manual", "scoped")
FIXTURES = (*LEAVES, "composed-macro")
STAGES = ("support", *FIXTURES)
_WRAPPERS = {
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
}


def _environment(source, env):
    """Reject inherited compiler flags or Cargo config that could redirect new work."""
    from composition_runner import check_counter_configuration

    upper = {key.upper(): value for key, value in env.items()}
    forbidden = _WRAPPERS | {
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_RUSTC",
    }
    for key, value in upper.items():
        if value and (key in forbidden or key.startswith(("CARGO_TARGET_", "CARGO_PROFILE_"))):
            raise ValueError(f"unexpected compiler setting in cold portfolio: {key}")
    cargo_home = Path(upper.get("CARGO_HOME", Path.home() / ".cargo")).resolve()
    check_counter_configuration(source, cargo_home)
    return {**env, **dict.fromkeys(_WRAPPERS, "")}


def _stages(fixtures):
    """A repeat of any small fixture still starts with a new shared graph."""
    selected = set(fixtures)
    if not selected or len(selected) != len(fixtures) or selected - set(FIXTURES):
        raise ValueError("expected a nonempty selection of distinct compile fixtures")
    return tuple(
        stage
        for stage in STAGES
        if stage in selected or (stage == "support" and selected.intersection(LEAVES))
    )


def locked_graphs(cargo, source, env, fixtures=FIXTURES):
    """Resolve each original Windows fixture from pinned sources, without compiling it."""
    graphs = {}
    for stage in _stages(fixtures):
        package = "rca-compile-fixture-support" if stage == "support" else "rca-compile-" + stage
        command = [
            str(cargo),
            "tree",
            "--manifest-path",
            str(source / "benches/compile/Cargo.toml"),
            "-p",
            package,
            "--target",
            "x86_64-pc-windows-msvc",
            "--locked",
            "--offline",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ]
        lines = subprocess.check_output(command, cwd=source, env=env, text=True, timeout=30).splitlines()
        packages = set()
        for line in lines:
            line = line.removesuffix(" (*)").replace(" (proc-macro)", "")
            match = re.fullmatch(r"([^ ]+) v([^ ]+)(?: \((.+)\))?", line)
            if not match:
                raise ValueError("unrecognised package in locked compile graph")
            name, version, folder = match.groups()
            packages.add(
                "local:" + (Path(folder).relative_to(source) / "Cargo.toml").as_posix()
                if folder is not None
                else registry.REGISTRY + "#" + name + "@" + version
            )
        if not packages:
            raise ValueError("empty locked compile graph")
        graphs[stage] = packages
    return graphs


def _absolute(value):
    path = PureWindowsPath(value)
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("expected an absolute Windows path without parent references")
    return path


def _stage(name, cargo, source, targets):
    package = "rca-compile-fixture-support" if name == "support" else "rca-compile-" + name
    target = targets / ("macro" if name == "composed-macro" else "shared")
    command = [
        str(cargo),
        "check",
        "--manifest-path",
        str(source / "benches/compile/Cargo.toml"),
        "-p",
        package,
        "--locked",
        "--offline",
        "-j",
        "1",
        "--message-format=json",
    ]
    return package, target, command


def _units(log, stage, source, target, shared, graph, *, standalone=False):
    """Require identical common Cargo units, with only the selected leaf rebuilt."""
    expected = source / "benches" / "compile" / stage / "Cargo.toml"
    kind, name = (
        ("lib", "rca_compile_fixture_support") if stage == "support" else ("bin", "rca-compile-" + stage)
    )
    cold = standalone or stage not in LEAVES
    units = set()
    reused = set()
    manifests = set()
    packages = set()
    root_count = 0
    for row in registry._records(log):
        if row.get("reason") != "compiler-artifact":
            continue
        manifest = _absolute(row["manifest_path"])
        if row["package_id"].startswith("path+"):
            if not manifest.is_relative_to(source) or not _absolute(row["target"]["src_path"]).is_relative_to(
                source
            ):
                raise ValueError("local compile unit came from another source")
            manifests.add(manifest)
            packages.add("local:" + manifest.relative_to(source).as_posix())
        elif not row["package_id"].startswith(registry.REGISTRY + "#"):
            raise ValueError("compile dependency is neither local nor from the locked registry")
        else:
            packages.add(row["package_id"])
        if not row["filenames"] or not all(_absolute(p).is_relative_to(target) for p in row["filenames"]):
            raise ValueError("compile output escaped the selected target")
        is_root = (
            manifest == expected
            and row["target"]["kind"] == [kind]
            and row["target"]["name"] == name
            and _absolute(row["target"]["src_path"])
            == expected.parent / "src" / ("lib.rs" if kind == "lib" else "main.rs")
        )
        root_count += is_root
        if row["fresh"] is not (not cold and not is_root):
            raise ValueError("unexpected Cargo reuse in cold compile portfolio")
        key = json.dumps({k: v for k, v in row.items() if k != "fresh"}, sort_keys=True)
        if key in units:
            raise ValueError("duplicate Cargo compile unit")
        units.add(key)
        if row["fresh"]:
            reused.add(key)
    required = {source / "rca-core/Cargo.toml", source / "benches/compile/support/Cargo.toml"}
    if stage == "composed-macro":
        required.update((source / "rca/Cargo.toml", source / "rca-macros/Cargo.toml"))
    if root_count != 1 or not required <= manifests or packages != graph or (not cold and reused != shared):
        raise ValueError("compile portfolio source, root or shared units differ")
    return units


def replay(directory, events, *, cargo, source, targets, capture_directory, graphs, fixtures=FIXTURES):
    """Replay raw jobs against graphs resolved from independently verified sources."""
    source, targets, capture_directory = map(_absolute, (source, targets, capture_directory))
    cargo = _absolute(cargo)
    shared = None
    durations = {}
    previous_job_end = -math.inf
    previous_exit = 0
    stages = _stages(fixtures)
    expected_destinations = [capture_directory / (name + ".txt") for name in stages]
    selected = [e for e in events if PureWindowsPath(e["destination"]).parent == capture_directory]
    if [(PureWindowsPath(e["destination"]), e["phase"]) for e in selected] != [
        (dest, phase) for dest in expected_destinations for phase in ("start", "finish")
    ]:
        raise ValueError("compile portfolio command order or completeness differs")

    for index, name in enumerate(stages):
        _, target, command = _stage(name, cargo, source, targets)
        log = directory / (name + ".txt")
        units = _units(log, name, source, target, shared, graphs[name])
        if name == "support":
            shared = units
        resource = json.loads(
            registry._regular(log.with_suffix(".resources.json")).read_text(encoding="utf-8")
        )
        evidence.validate_windows_resource_counters(resource, expected_kind="compile", rank_cold=True)
        start, finish = selected[2 * index : 2 * index + 2]
        receipt = resource["process_receipt"]
        clocks = [event.get("monotonic") for event in (start, finish)]
        lifetime = resource["direct_process"]
        if (
            any(type(v) not in (float, int) or not math.isfinite(v) for v in clocks)
            or not previous_job_end <= clocks[0] < clocks[1]
            or not previous_exit <= lifetime["CreationTime100ns"] < lifetime["ExitTime100ns"]
        ):
            raise ValueError("compile portfolio jobs overlap or repeat an earlier process")
        previous_job_end, previous_exit = clocks[1], lifetime["ExitTime100ns"]
        if (
            start["command"] != command
            or _absolute(start["cwd"]) != source
            or _absolute(start["target"]) != target
            or _absolute(start["build_dir"]) != target
            or start["incremental"] != "0"
            or _absolute(receipt["build_dir"]) != target
            or finish.get("returncode") != 0
            or "error" in finish
            or finish["process_receipt"] != receipt
            or start["job_memory_limit_bytes"] != receipt["job_memory_limit_bytes"]
            or finish["job_memory_limit_bytes"] != receipt["job_memory_limit_bytes"]
        ):
            raise ValueError("compile portfolio command or guarded process receipt differs")
        durations[name] = resource["real_seconds"]

    # Each shared + leaf value adds two measured Cargo leader lifetimes, not
    # an estimate of a standalone command. The three values share one build.
    return {
        "stages_seconds": durations,
        "fixture_seconds": {
            name: durations[name] + (durations["support"] if name in LEAVES else 0.0)
            for name in stages
            if name != "support"
        },
    }


def capture(monitor, cargo, source, directory, targets, env, fixtures=FIXTURES):
    """Run a single guarded Windows observation with two initially absent targets."""
    if not monitor.experimental_rank:
        raise ValueError("compile portfolio requires guarded build-directory receipts")
    env = _environment(source, env)
    if directory == targets or directory.is_relative_to(targets) or targets.is_relative_to(directory):
        raise ValueError("compile transcripts and disposable targets must be separate")
    for path in (source, directory, targets):
        if not path.is_absolute() or path.resolve() != path:
            raise ValueError("compile portfolio requires direct absolute paths")
    graphs = locked_graphs(cargo, source, env, fixtures)
    directory.mkdir(parents=True, exist_ok=False)
    targets.mkdir(parents=True, exist_ok=False)
    owned = targets.lstat()
    try:
        for name in _stages(fixtures):
            _, target, command = _stage(name, cargo, source, targets)
            monitor.run(
                command,
                directory / (name + ".txt"),
                source,
                {
                    **env,
                    "CARGO_TARGET_DIR": str(target),
                    "CARGO_BUILD_BUILD_DIR": str(target),
                    "CARGO_INCREMENTAL": "0",
                },
            )
        events = [
            json.loads(line)
            for line in (monitor.directory / "processes.jsonl").read_text(encoding="utf-8").splitlines()
        ]
        return replay(
            directory,
            events,
            cargo=cargo,
            source=source,
            targets=targets,
            capture_directory=directory,
            graphs=graphs,
            fixtures=fixtures,
        )
    finally:
        # This call created the absent directory. Never delete a replacement link.
        if (
            targets.is_symlink()
            or targets.resolve() != targets
            or not os.path.samestat(owned, targets.lstat())
        ):
            raise ValueError("compile portfolio target was replaced during capture")
        shutil.rmtree(targets)


_FIRST_ORDER = ("control-a", "control-b", "base", "candidate")
_ORDERS = (_FIRST_ORDER, tuple(reversed(_FIRST_ORDER)))


def screen_cold_pairs(blocks):
    """Screen two reversed orders. A noisy control or a split result needs wider qualification."""
    if len(blocks) != 2 or any(
        set(block) != set(_FIRST_ORDER)
        or any(type(value) not in (float, int) or not math.isfinite(value) or value <= 0
               for value in block.values())
        for block in blocks
    ):
        raise ValueError("expected two complete cold-build blocks with positive durations")
    limit = evidence.REGRESSION_LIMIT
    ratios = [dict(candidate_over_base=b["candidate"] / b["base"],
                   control_spread=max(b["control-a"], b["control-b"]) / min(b["control-a"], b["control-b"]))
              for b in blocks]
    if any(not math.isfinite(value) for row in ratios for value in row.values()):
        raise ValueError("cold-build ratio overflow")
    status = "inconclusive"
    if all(r["control_spread"] <= limit for r in ratios):
        if all(r["candidate_over_base"] > limit for r in ratios):
            status = "fail"
        elif all(r["candidate_over_base"] <= limit for r in ratios):
            status = "pass"
    return {"decision": status, "limit": limit, "ratios": ratios, "release_authorised": False,
            "metric": "standalone cold Cargo check, including owned Job cleanup"}


def _require_outer_guard():
    """Refuse standalone invocation outside the operator's two-GiB Windows Job."""
    import windows_process as wp

    api = wp.WindowsProcessApi()
    if not api.in_job(api.kernel.GetCurrentProcess(), None):
        raise ValueError("run this comparison inside the reviewed H1 memory guard")
    limits = wp.JobExtendedLimits()
    api.checked("QueryInformationJobObject", None, 9, ctypes.byref(limits), ctypes.sizeof(limits), None)
    flags = limits.BasicLimitInformation.LimitFlags
    if flags & 0x2200 != 0x2200 or not 0 < limits.JobMemoryLimit <= 2 * 1024**3:
        raise ValueError("enclosing Windows Job must cap committed memory and kill children on close")


def _confirmation_decision(pairs, protocol):
    """Use the original twenty-block rule. A prefix can rule out a pass, never prove one."""
    if set(pairs) != {"candidate-reference", "reference-reference-aa"} or len({len(rows) for rows in pairs.values()}) != 1:
        raise ValueError("expected matched candidate and control blocks")
    full = len(next(iter(pairs.values()))) == len(evidence.seeded_order_schedule(protocol))
    score = evidence.paired_ratio_statistics if full else evidence.paired_prefix_bound
    scores = {mode: score(rows, "maximum", protocol) for mode, rows in pairs.items()}
    rejected = {mode for mode, result in scores.items()
                if result and (not result["passed"] if full else result["irrecoverable"])}
    if "reference-reference-aa" in rejected:
        decision = "inconclusive"
    elif "candidate-reference" in rejected:
        decision = "fail"
    elif full:
        decision = "pass"
    else:
        return None
    return dict(decision=decision, statistics=scores, controls_complete=full,
                control_status=("rejected" if "reference-reference-aa" in rejected
                                else "passed" if full else "incomplete"),
                ratios={m: [row["subject"] / row["control"] for row in rows] for m, rows in pairs.items()},
                metric="standalone cold Cargo check, elapsed time until Cargo exits",
                release_authorised=False)


def _confirm_fixture(fixture, output, tmp, roots, env, cargo, monitor, plan):
    """Start fresh with the original seeded schedule, command, timer and independent controls."""
    import composition_runner as runner

    protocol = monitor.protocol
    schedule = evidence.seeded_order_schedule(protocol)
    controls = {}
    for role in ("subject", "control"):
        controls[role] = tmp / ("independent-" + role)
        shutil.copytree(roots["base"], controls[role])
    sources = {"reference-reference-aa": controls, "candidate-reference": {
        "subject": roots["candidate"], "control": roots["base"]}}
    if any(key.upper() == "CARGO_BUILD_BUILD_DIR" and val for key, val in env.items()):
        raise ValueError("ambient Cargo build directory is not permitted in cold confirmation")
    runner.write_json(output / "confirmation-plan.json", dict(plan, schedule=schedule,
                      modes=[evidence.interleaved_mode_order(protocol, n) for n in range(1, len(schedule) + 1)],
                      protocol_sha256=evidence._sha256(runner.WINDOWS_PROTOCOL)))
    for block, order in enumerate(schedule, 1):
        pairs = {}
        for mode in evidence.interleaved_mode_order(protocol, block):
            if shutil.disk_usage(tmp).free < 24 * 1024**3:
                raise ValueError("cold-build disk reserve lost")
            paired = output / mode
            runner.execute_pairs(protocol, sources[mode], {}, tmp / mode, paired, cargo,
                                 {**env, "CARGO_NET_OFFLINE": "true"}, monitor,
                                 blocks=[(block, order)], workloads={f"compile/{fixture}"})
            pairs[mode] = evidence.load_compile_pairs(paired / "subject", paired / "control",
                                                      fixture, protocol, block_count=block)
        decision = _confirmation_decision(pairs, protocol)
        if decision is not None:
            base_manifest = runner.source_hashes(roots["base"])
            if any(runner.source_hashes(path) != base_manifest for path in controls.values()):
                raise ValueError("independent control sources changed")
            return dict(decision, blocks_measured=block)


def _screen_fixture(fixture, output, tmp, roots, graphs, env, cargo, monitor):
    """Check both run orders from distinct clean targets and score complete owned Jobs."""
    blocks = []
    previous_exit = 0
    for index, order in enumerate(_ORDERS, 1):
        block = {}
        for arm in order:
            role = "candidate" if arm == "candidate" else "base"
            source = roots[role]
            if shutil.disk_usage(output).free < 24 * 1024**3:
                raise ValueError("cold-build disk reserve lost")
            with tempfile.TemporaryDirectory(prefix="target-", dir=tmp) as target_path:
                target = Path(target_path)
                _, _, command = _stage(fixture, cargo, source, target.parent)
                log = output / f"{index}-{arm}.txt"
                monitor.run(command, log, source, {**env, "CARGO_TARGET_DIR": str(target),
                            "CARGO_BUILD_BUILD_DIR": str(target), "CARGO_INCREMENTAL": "0"})
                _units(log, fixture, source, target, None, graphs[role], standalone=True)
                resource = json.loads(log.with_suffix(".resources.json").read_bytes())
                evidence.validate_windows_resource_counters(resource, expected_kind="compile", rank_cold=True)
                proc = resource["direct_process"]
                receipt = resource["process_receipt"]
                if (not previous_exit <= proc["CreationTime100ns"] < proc["ExitTime100ns"]
                        or Path(receipt["build_dir"]) != target):
                    raise ValueError("wrong, repeated or overlapping cold-build process")
                previous_exit = proc["ExitTime100ns"]
                block[arm] = receipt["elapsed_seconds"]
        blocks.append(block)
    return dict(screen_cold_pairs(blocks), blocks=blocks, orders=_ORDERS)


def capture_pr(base, candidate, fixture, output, *, confirm=None, confirm_sha256=None):
    """Compare trusted local Git objects on H1 under the existing outer memory guard."""
    import check_windows_performance_inputs as identity
    import composition_runner as runner

    started = time.monotonic()
    if sys.platform != "win32" or fixture not in FIXTURES:
        raise ValueError("the cold-build screen requires Windows and a known fixture")
    _require_outer_guard()
    host = runner.windows_host()
    env, tools = runner.tool_environment(include_capture_tool=False)
    if "host: x86_64-pc-windows-msvc" not in runner.read([tools["rustc"], "-vV"]).splitlines():
        raise ValueError("cold-build comparison requires the pinned MSVC toolchain")
    pins = {name: evidence._sha256(Path(path)) for name, path in tools.items()}
    protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
    revisions = {role: identity.signed_source(revision)[0] for role, revision in
                 (("base", base), ("candidate", candidate))}
    output = output.absolute()
    if output != output.resolve() or not output.parent.is_dir() or output.is_relative_to(runner.ROOT):
        raise ValueError("use a new evidence directory outside the checkout")
    if shutil.disk_usage(output.parent).free < 24 * 1024**3:
        raise ValueError("H1 cold builds require 24 GiB of free disk")

    lock = Path(tempfile.gettempdir()) / runner.LOCK.name
    with runner.machine_lock(lock):
        output.mkdir()
        monitor = runner.WindowsMonitor(output / "telemetry", protocol, cargo=tools["cargo"],
                                        complete=confirm is not None, experimental_rank=confirm is None)
        with tempfile.TemporaryDirectory(prefix="cold-pr-", dir=output) as tmp:
            roots = {}
            graphs = {}
            manifests = {}
            for role, revision in (("base", base), ("candidate", candidate)):
                source = Path(tmp) / role
                source.mkdir()
                with tarfile.open(fileobj=io.BytesIO(identity.git("archive", "--format=tar", revision))) as ar:
                    ar.extractall(source, filter="data")
                roots[role] = source
                manifests[role] = runner.source_hashes(source)
                graphs[role] = locked_graphs(tools["cargo"], source, _environment(source, env), (fixture,))[fixture]
            # A changed consumer or graph answers another question. Escalate rather
            # than calling incomparable compiler work a PR improvement.
            prefixes = (f"benches/compile/{fixture}/", "benches/compile/support/")
            def workload(role):
                return {k: v for k, v in manifests[role].items()
                        if k.startswith(prefixes) or k == "benches/compile/Cargo.toml"}
            if workload("base") != workload("candidate") or graphs["base"] != graphs["candidate"]:
                raise ValueError("consumer workload or compiler graph changed: use wider qualification")
            inputs = dict(fixture=fixture, base=base, candidate=candidate, trees=revisions, host=host, tools=pins)
            if confirm is not None:
                raw = confirm.read_bytes()
                if hashlib.sha256(raw).hexdigest() != confirm_sha256:
                    raise ValueError("original screen differs from the independently selected SHA-256")
                previous = json.loads(raw)
                screened = screen_cold_pairs(previous.get("blocks", []))
                if (screened["decision"] != "inconclusive" or any(previous.get(k) != v for k, v in
                        (inputs | screened | {"orders": list(map(list, _ORDERS))}).items())):
                    raise ValueError("confirmation requires an inconclusive screen for exactly these inputs")
                result = _confirm_fixture(fixture, output, Path(tmp), roots, env, tools["cargo"], monitor,
                                          dict(inputs, screen_sha256=confirm_sha256))
            else:
                result = _screen_fixture(fixture, output, tmp, roots, graphs, env, tools["cargo"], monitor)
            if any(runner.source_hashes(src) != manifests[role] for role, src in roots.items()):
                raise ValueError("measured source changed")
        runner.check_host_unchanged(host, monitor)
        if pins != {name: evidence._sha256(Path(path)) for name, path in tools.items()}:
            raise ValueError("compiler tools changed")
        result.update(inputs, elapsed_seconds=time.monotonic() - started)
        runner.write_json(output / "screen.json", result)
        return result


def main(argv=None):
    """Return a decision exit or, for an enclosing Job, a collection exit."""
    parser = argparse.ArgumentParser(description="Guarded H1 PR screen; not a release verdict.")
    parser.add_argument("--base", required=True, help="full independently selected signed baseline commit")
    parser.add_argument("--candidate", required=True, help="full independently selected signed candidate commit")
    parser.add_argument("--fixture", required=True, choices=FIXTURES)
    parser.add_argument("--output", required=True, type=Path, help="new directory for raw transcripts and decision")
    parser.add_argument("--collect-only", action="store_true",
                        help="exit zero for any completed measurement; read screen.json after the outer guard exits")
    parser.add_argument("--confirm", type=Path, metavar="SCREEN_JSON",
                        help="check an inconclusive fixture again using the original seeded twenty-block rule")
    parser.add_argument("--confirm-sha256", help="independently recorded hash of the first inconclusive screen")
    args = parser.parse_args(argv)
    if bool(args.confirm) != bool(args.confirm_sha256):
        parser.error("--confirm and --confirm-sha256 must be supplied together")
    try:
        verdict = capture_pr(args.base, args.candidate, args.fixture, args.output, confirm=args.confirm,
                             confirm_sha256=args.confirm_sha256)
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        print(json.dumps({"decision": "inconclusive", "release_authorised": False, "reason": str(exc)}))
        return 2
    print(json.dumps(verdict, allow_nan=False))
    return 0 if args.collect_only else {"pass": 0, "fail": 1, "inconclusive": 2}[verdict["decision"]]


if __name__ == "__main__":
    sys.exit(main())
