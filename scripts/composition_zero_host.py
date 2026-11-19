"""Opt-in Windows debug allocation observation. Does not select a release verdict.

The signed protocol's zero-host build and replay remain independent. A caller
must supply its verified subject receipt and retain the guarded raw commands.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tomllib

import composition_compile_portfolio as portfolio
import composition_registry_seed as registry
import composition_runner as runner
import composition_windows_absolute as absolute


PACKAGE = "rca-qualification-zero"
HOST = "bench-zero-host"
FEATURES = ["allocation-measurement", "default"]
FIXTURES = {
    "main.rs": "rca-core/tests/composition_qualification/main.rs",
    "composition-zero-runner.rs": "rca-core/tests/composition_qualification/composition-zero-runner.rs",
}
TEMPLATE = Path(__file__).parent / "fixtures/composition-zero-host.toml"


def _tree(cargo, source, env, package, *, original):
    """Resolve feature-unified Windows packages without building or fetching any."""
    command = [str(cargo), "tree", "--offline", "--target", "x86_64-pc-windows-msvc",
               "-p", package, "--features", "allocation-measurement", "--edges",
               "normal,build,dev" if original else "normal,build", "--prefix", "none",
               "--format", "{p}|{f}"]
    if original:
        command.append("--locked")
    output = subprocess.check_output(command, cwd=source, env=env, text=True, timeout=30)
    graph = {}
    for line in output.splitlines():
        package, features = line.removesuffix(" (*)").split("|")
        match = re.fullmatch(r"([^ ]+) v([^ ]+)(?: \((.+)\))?", package.replace(" (proc-macro)", ""))
        if not match:
            raise ValueError("unexpected zero-host package identity")
        name, version, folder = match.groups()
        identity = str(Path(folder)) if folder else f"{registry.REGISTRY}#{name}@{version}"
        graph.setdefault(identity, set()).add(tuple(sorted(filter(None, features.split(",")))))
    if not graph:
        raise ValueError("unexpected zero-host dependency tree")
    return graph


def _locked_packages(before, after):
    """Permit only the new workspace member in Cargo's offline lockfile update."""
    hosts = [p for p in after["package"] if p["name"] == PACKAGE]
    if len(hosts) != 1 or set(hosts[0]) != {"name", "version", "dependencies"} or (
        hosts[0]["version"] != "0.0.0"
        or {**after, "package": [p for p in after["package"] if p not in hosts]} != before
    ):
        raise ValueError("zero host changed existing locked packages")


def prepare(cargo, receipt, destination, env):
    """Prepare a fresh copy, leaving the verified subject's files and lock intact."""
    staged = runner.validate_staged_source(receipt)
    original = Path(staged["root"])
    if (original.resolve() != original or destination.resolve() != destination
            or destination.is_relative_to(original)):
        raise ValueError("zero host requires a direct directory outside the subject")
    protocol = runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL)["common_workloads"]["files"]
    for name in FIXTURES.values():
        if staged["effective"].get(name) != protocol[name]["sha256"]:
            raise ValueError("zero workload or synchronous allocation runner changed")
    shutil.copytree(original, destination, symlinks=True)
    if any(p.is_symlink() or p.resolve() != p for p in destination.rglob("*")) or (
        runner.source_hashes(destination) != staged["effective"]
    ):
        raise ValueError("zero host source copy differs from the verified subject")
    host = destination / HOST
    manifest = destination / "Cargo.toml"
    lock = destination / "Cargo.lock"
    original_lock = lock.read_bytes()
    if hashlib.sha256(original_lock).hexdigest() != staged["effective"]["Cargo.lock"]:
        raise ValueError("original zero subject lock changed before adaptation")
    before = tomllib.loads(original_lock.decode("utf-8"))
    workspace = manifest.read_bytes()
    marker = b"members = [\n"
    if workspace.count(marker) != 1:
        raise ValueError("unexpected workspace member list")
    if hashlib.sha256(workspace).hexdigest() != staged["effective"]["Cargo.toml"]:
        raise ValueError("original zero subject manifest changed before adaptation")
    graph = _tree(cargo, destination, env, "rca-core", original=True)
    host.mkdir()
    adapted = workspace.replace(marker, marker + f'    "{HOST}",\n'.encode())
    manifest.write_bytes(adapted)
    expected = {**staged["effective"], "Cargo.toml": hashlib.sha256(adapted).hexdigest(),
                f"{HOST}/Cargo.toml": runner.evidence._sha256(TEMPLATE)}
    shutil.copyfile(TEMPLATE, host / "Cargo.toml")
    for name, relative in FIXTURES.items():
        shutil.copyfile(destination / relative, host / name)
        expected[f"{HOST}/{name}"] = protocol[relative]["sha256"]
    narrow = _tree(cargo, destination, env, PACKAGE, original=False)
    new_lock = lock.read_bytes()
    _locked_packages(before, tomllib.loads(new_lock.decode("utf-8")))
    expected["Cargo.lock"] = hashlib.sha256(new_lock).hexdigest()
    _features_match(graph, narrow, host)
    if runner.source_hashes(destination) != expected:
        raise ValueError("adapted zero source differs from the subject and allowed additions")
    return expected, {identity: next(iter(features)) for identity, features in narrow.items()}


def _features_match(original, narrow, host):
    """Reject ambiguous normal/build feature sets instead of guessing their roles."""
    if narrow.get(str(host)) != {tuple(FEATURES)} or any(
        len(features) != 1 or original.get(identity) != features
        for identity, features in narrow.items() if identity != str(host)
    ):
        raise ValueError("retained zero-host dependency features differ or are ambiguous")


def _binary(log, source, target, graph):
    """Check the compiled product, allocator, test profile and selected fixture."""
    required = {"rca_core": source / "rca-core/src/lib.rs", "composition_zero": source / HOST / "main.rs"}
    found = []
    allocators = 0
    packages = set()
    for row in registry._records(log):
        if row["reason"] != "compiler-artifact":
            continue
        manifest = Path(row["manifest_path"])
        info = row["target"]
        local = row["package_id"].startswith("path+")
        identity = str(manifest.parent) if local else row["package_id"]
        if graph.get(identity) != tuple(row["features"]):
            raise ValueError("compiled zero-host dependency features differ from the original graph")
        packages.add(identity)
        host = info["name"] == "composition_zero"
        if row["fresh"] is not False or row["profile"] != {
            "opt_level": "1" if local else "3", "debuginfo": 0, "debug_assertions": True,
            "overflow_checks": True, "test": host,
        }:
            raise ValueError("zero host needs fresh compilation with its original debug profile")
        if local and not (manifest.is_relative_to(source) and
                          Path(info["src_path"]).is_relative_to(manifest.parent)):
            raise ValueError("local zero-host input escaped its subject")
        if info["name"] in required:
            name = info["name"]
            expected_manifest = source / (HOST if host else "rca-core") / "Cargo.toml"
            if (manifest != expected_manifest or Path(info["src_path"]) != required[name]
                    or info["kind"] != (["test"] if host else ["lib"]) or row["features"] != FEATURES):
                raise ValueError("zero host compiled the wrong source or features")
            found.append(name)
        if info["name"] == "dhat" and info["kind"] == ["lib"]:
            allocators += row["package_id"].startswith(registry.REGISTRY + "#dhat@")
    if sorted(found) != sorted(required) or allocators != 1 or packages != set(graph):
        raise ValueError("zero host must compile the allocator, subject and test exactly once")
    return absolute._artifact(log, target, "composition_zero", "test", required["composition_zero"],
                              allocation=True)


def capture(monitor, cargo, receipt, raw, scratch, env):
    """Run both existing oracles in an isolated test host. Leave the legacy lane untouched."""
    if not monitor.experimental_rank or not monitor.complete or str(cargo) != monitor.cargo:
        raise ValueError("zero host requires full Windows process receipts")
    if (scratch.resolve() != scratch or raw.resolve() != raw or scratch.is_relative_to(raw)
            or raw.is_relative_to(scratch)):
        raise ValueError("zero host needs separate direct scratch and raw directories")
    receipt_hash = runner.evidence._sha256(registry._regular(receipt))
    origin = Path(json.loads(receipt.read_text(encoding="utf-8"))["root"])
    if raw.is_relative_to(origin) or scratch.is_relative_to(origin):
        raise ValueError("zero scratch and raw evidence must not modify the original source")
    env = portfolio._environment(origin, env)
    runner.check_counter_configuration(scratch / "source", Path(env.get("CARGO_HOME", Path.home() / ".cargo")))
    raw.mkdir()
    scratch.mkdir()
    owned = scratch.stat()
    source, target = scratch / "source", scratch / "target"
    try:
        expected, graph = prepare(cargo, receipt, source, env)
        run_env = {**env, "CARGO_TARGET_DIR": str(target), "CARGO_BUILD_BUILD_DIR": str(target),
                   "CARGO_INCREMENTAL": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
                   "CARGO_PROFILE_TEST_OPT_LEVEL": "1"}
        build = raw / "zero-build.jsonl"
        monitor.run([str(cargo), "test", "-p", PACKAGE, "--test", "composition_zero",
                     "--features", "allocation-measurement", "--offline", "--locked", "-j", "1",
                     "--no-run", "--message-format=json"], build, source, run_env)
        binary = _binary(build, source, target, graph)
        image_hash = runner.evidence._sha256(binary)
        results = {}
        for label, name in absolute.ZERO_TESTS.items():
            log = raw / f"zero-{label}.txt"
            if runner.evidence._sha256(registry._regular(binary)) != image_hash:
                raise ValueError("compiled zero image changed between build and execution")
            monitor.run([str(binary), name, "--exact", "--nocapture"], log, source, run_env)
            results[label] = absolute._zero_test(log, name)
        if (runner.evidence._sha256(registry._regular(receipt)) != receipt_hash
                or runner.source_hashes(source) != expected
                or runner.evidence._sha256(registry._regular(binary)) != image_hash):
            raise ValueError("zero source or compiled image changed during capture")
        runner.write_json(raw / "zero-inputs.json", {
            "source_receipt_sha256": receipt_hash,
            "workspace_sha256": expected["Cargo.toml"], "lock_sha256": expected["Cargo.lock"],
            "host_inputs": {name: expected[f"{HOST}/{name}"] for name in ("Cargo.toml", *FIXTURES)},
            "executable_sha256": image_hash,
        })
        return results
    finally:
        if scratch.resolve() != scratch or not os.path.samestat(owned, scratch.lstat()):
            raise ValueError("zero host scratch was replaced during capture")
        shutil.rmtree(scratch)
