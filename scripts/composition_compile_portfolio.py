"""Measure two cold Cargo graphs and the first check of each unbuilt fixture.

One observation is not a pass. The caller owns sample selection, signed source
and host checks, independent controls, and the comparison with release limits.
"""

import json
import math
import os
from pathlib import Path, PureWindowsPath
import re
import shutil
import subprocess

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


def _units(log, stage, source, target, shared, graph):
    """Require identical common Cargo units, with only the selected leaf rebuilt."""
    expected = source / "benches" / "compile" / stage / "Cargo.toml"
    kind, name = (
        ("lib", "rca_compile_fixture_support") if stage == "support" else ("bin", "rca-compile-" + stage)
    )
    cold = stage not in LEAVES
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
