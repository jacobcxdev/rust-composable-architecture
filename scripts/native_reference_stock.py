"""Admit fixed stock Windows reference executables for fresh measurements, never candidate products."""

import hashlib
import json
from pathlib import Path

from composition_performance import RELEASE_BASELINE_REVISION, RUST100_TOOL_VERSIONS
from composition_registry_seed import _regular, _sha
from composition_runner import windows_provider
from qualification_reference_cache import verify_reference_products


provider = windows_provider()
REFERENCE_TREE = "843d91754520b7bcf57dde6e46caea10b61ccf45"
PRODUCTS = ("native-normal", "native-allocation", "deterministic-normal", "deterministic-allocation")
WORKLOAD = {"harness_sha256": "rca-tokio/benches/qualification.rs",
            "harness_module_sha256": "rca-tokio/benches/qualification/shutdown.rs",
            "scenario_manifest_sha256": "docs/performance/scenarios.txt"}
HOST_FIELDS = ("system", "os", "machine_model", "cpu", "hostname", "python", "python_executable", "logical_cpus")


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()).hexdigest()


def inputs(metadata, build_tools, host):
    """Derive cache keys from authenticated stock build metadata and the installed compiler binaries.

    The caller supplies the verified build's tool records under cargo/rustc/rustdoc.
    No original build paths, timestamps, candidate revisions or measured results form part of the key.
    """
    provider.validate_metadata(metadata)
    reference = metadata["reference"]
    if (reference["revision"] != RELEASE_BASELINE_REVISION or reference["tree"] != REFERENCE_TREE
            or metadata.get("git_source_verification", {}).get("reference") != reference
            or type(metadata.get("build_exit_code")) is not int or metadata["build_exit_code"] != 0
            or metadata.get("profile_environment") != {}):
        raise ValueError("expected a successful, Git-verified stock reference build with no profile overrides")
    toolchain = metadata["toolchain"]
    if (any(toolchain[k] != RUST100_TOOL_VERSIONS[k] for k in ("cargo", "rustc"))
            or toolchain["rustc_commit"] != "0ef1bc386059596dada8fb65b50425aab2ea637a"):
        raise ValueError("reference requires the stock Windows compiler and Cargo")

    tools = {}
    for name in ("cargo", "rustc", "rustdoc"):
        record = build_tools[name]
        path = _regular(Path(record["path"]))
        if path.name.lower() != name + ".exe" or _sha(path) != record["sha256"]:
            raise ValueError(f"installed reference tool differs from the build: {name}")
        tools[name] = {key: record[key] for key in ("path", "sha256")}
    if (host.get("system") != "Windows" or type(host.get("logical_cpus")) is not int or host["logical_cpus"] < 1
            or any(not isinstance(host.get(k), str) or not host[k] for k in HOST_FIELDS[:-1])):
        raise ValueError("incomplete stock Windows host cohort")

    commands = {row["binary_role"]: row for row in metadata["commands"]}
    configurations = {}
    for product in PRODUCTS:
        role = "reference_" + product.removeprefix("native-").replace("-", "_")
        row = commands[role]
        args = ["bench", "--locked", "--offline", "-j1", "-p", "rca-tokio", "--bench", "qualification",
                "--no-run", "--message-format=json"]
        if product.endswith("allocation"):
            args += ["--features", "qualification-dhat"]
        if row.get("command") != [tools["cargo"]["path"], *args]:
            raise ValueError(f"unexpected stock reference Cargo command: {role}")
        configurations[product] = {"arguments": args, **{
            artifact: {key: row[artifact][key] for key in ("features", "profile")}
            for artifact in ("cargo_artifact", "core_artifact")}}

    binaries = [metadata["binaries"][key] for key in ("reference_normal", "reference_deterministic_normal")]
    return {"tree": reference["tree"],
            "effective_source_sha256": _digest([b["source_manifest_sha256"] for b in binaries]),
            "lock_sha256": _digest([b["lock_sha256"] for b in binaries]),
            "workload_sha256": _digest({key: metadata[key] for key in WORKLOAD}),
            "toolchain_sha256": _digest([toolchain, tools]),
            "configuration_sha256": _digest([metadata["configuration"], metadata["profile_environment"], configurations]),
            "platform_sha256": _digest({key: host[key] for key in HOST_FIELDS})}


def load_reference_products(cache, *, enrolment_sha256, metadata, build_tools, host, workload_root=None):
    """Check current workloads and host before resolving both independent sets against an external trust pin."""
    current = inputs(metadata, build_tools, host)
    if provider.host_metadata() != host:
        raise ValueError("stock reference host differs from the current Windows host")
    root = provider.ROOT if workload_root is None else Path(workload_root)
    for key, relative in WORKLOAD.items():
        if _sha(_regular(root / relative)) != metadata[key]:
            raise ValueError(f"current stock workload differs from the build: {relative}")
    return verify_reference_products(cache, enrolment_sha256=enrolment_sha256, inputs=current,
                                     purpose="native-stock", names=PRODUCTS, roles=("a", "b"))
