"""Retain Windows allocation and stripped-PE observations for existing budgets.

This does not compare against macOS budgets or claim that compiled PE files ran.
"""

import json
from pathlib import Path
import re
import shutil

import composition_performance as evidence
import composition_runner as runner
import windows_strip


ZERO_TESTS = {
    "direct_scope": "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
    "combine": "composition::combine::tests::combine_all_none_widths_do_not_allocate",
}
BINARY_NAMES = (
    "direct", "composed-manual", "composed-macro", "scoped",
    "deterministic-composition-test", "tokio-application",
)


def allocation_observations(path):
    """Require every manifest row before selecting the five gated workloads."""
    return evidence._parse_allocations(Path(path), evidence.DEFAULT_ALLOCATION_MANIFEST)


def selected_allocation_values(rows, zero_tests):
    """Keep the existing thirteen budget identities without applying their limits."""
    if set(zero_tests) != set(ZERO_TESTS) or any(value is not True for value in zero_tests.values()):
        raise ValueError("both zero-allocation tests must pass")
    document = evidence.validate_budget_document(runner.BUDGET)
    result = {}
    for row in document["rows"]:
        identity = row["scenario"]
        if not identity.startswith("allocation/"):
            continue
        parts = identity.split("/")
        if len(parts) == 2:
            result[identity] = 0
        else:
            scenario, statistic = parts[1:]
            result[identity] = rows[scenario][statistic]
    if len(result) != 13:
        raise ValueError("expected thirteen existing allocation gates")
    return result


def _artifact(log, target, name, kind, source):
    """Bind one fresh Cargo executable to the isolated target directory."""
    found = []
    for line in log.read_text(encoding="utf-8").splitlines():
        if not line.startswith("{"):
            continue
        record = json.loads(line)
        info = record.get("target", {})
        if (record.get("reason") == "compiler-artifact" and info.get("name") == name
                and kind in info.get("kind", ()) and record.get("executable")):
            if record.get("fresh") is not False:
                raise ValueError(f"{name} executable was not freshly compiled")
            if Path(info.get("src_path", "")).resolve() != Path(source).resolve():
                raise ValueError(f"{name} artifact source changed")
            executable = Path(record["executable"])
            if executable.is_symlink():
                raise ValueError(f"{name} artifact became a symlink")
            found.append(executable.resolve())
    if (len(found) != 1 or not found[0].is_relative_to(target.resolve())
            or not found[0].is_file() or found[0].is_symlink()):
        raise ValueError(f"expected one fresh isolated {name} executable")
    return found[0]


def _zero_test(log, name):
    """Require one exact success from the maintained runner or ordinary libtest."""
    text = log.read_text(encoding="utf-8")
    selected = re.findall(r"^test (\S+) \.\.\. (\S+)$", text, re.MULTILINE)
    summaries = [line for line in text.splitlines() if line.startswith("test result:")]
    summary = re.fullmatch(
        r"test result: (\w+)\. (\d+) passed; (\d+) failed"
        r"(?:; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in [^\r\n]+)?",
        summaries[0],
    ) if len(summaries) == 1 else None
    if (selected != [(name, "ok")] or summary is None
            or summary.groups()[:3] != ("ok", "1", "0")
            or summary.groups()[3:5] not in ((None, None), ("0", "0"))):
        raise ValueError(f"zero-allocation test did not pass exactly: {name}")
    filtered = summary.group(6)
    return {"test": name, "passed": 1, "failed": 0,
            "filtered_out": int(filtered) if filtered is not None else None,
            "stdout_sha256": evidence._sha256(log)}


def _pe_identity(binary):
    """Require an x64 PE executable with no remaining COFF symbol table."""
    data = binary.read_bytes()
    if len(data) < 0x40 or data[:2] != b"MZ":
        raise ValueError("stripped binary is not PE")
    offset = int.from_bytes(data[0x3c:0x40], "little")
    if offset + 24 > len(data) or data[offset:offset + 4] != b"PE\0\0":
        raise ValueError("stripped binary has no PE header")
    header = data[offset + 4:offset + 24]
    machine = int.from_bytes(header[:2], "little")
    symbol_offset = int.from_bytes(header[8:12], "little")
    symbol_count = int.from_bytes(header[12:16], "little")
    optional_size = int.from_bytes(header[16:18], "little")
    characteristics = int.from_bytes(header[18:20], "little")
    if machine != 0x8664 or symbol_offset or symbol_count or not characteristics & 0x0002:
        raise ValueError("stripped binary is not a symbol-free x64 PE executable")
    optional = data[offset + 24:offset + 24 + optional_size]
    if (len(optional) != optional_size or optional_size < 0xa8
            or int.from_bytes(optional[:2], "little") != 0x20b
            or int.from_bytes(optional[0x6c:0x70], "little") < 7
            or int.from_bytes(optional[0xa4:0xa8], "little") != 0):
        raise ValueError("stripped PE retains a debug directory or invalid PE32+ header")
    return len(data)


def strip_pe(binary, label, directory, env, monitor, tools):
    """Retain original and stripped PE bytes independently of build caches."""
    binary = Path(binary)
    if label not in BINARY_NAMES or not binary.is_file() or binary.is_symlink():
        raise ValueError("missing owned PE binary")
    directory = Path(directory)
    retained = directory / "binaries"
    retained.mkdir(exist_ok=True)
    original = retained / f"{label}-original.exe"
    intermediate = retained / f"{label}-llvm-stripped.exe"
    stripped = retained / f"{label}-stripped.exe"
    if any(path.exists() or path.is_symlink() for path in (original, intermediate, stripped)):
        raise ValueError("PE evidence already exists")
    before_bytes = binary.stat().st_size
    before_sha256 = evidence._sha256(binary)
    shutil.copyfile(binary, original)
    if evidence._sha256(original) != before_sha256:
        raise ValueError("copied PE changed before stripping")
    log = directory / f"strip-{label}.txt"
    monitor.run([tools["objcopy"], "--strip-all", str(original), str(intermediate)], log, directory, env)
    removal = windows_strip.finish_strip(intermediate, stripped, directory / f"debug-strip-{label}.txt",
                                         directory, env, monitor, tools)
    size = _pe_identity(stripped)
    if evidence._sha256(binary) != before_sha256 or evidence._sha256(original) != before_sha256:
        raise ValueError("original PE changed during stripping")
    if size > before_bytes:
        raise ValueError("PE grew during stripping")
    row = {"bytes": size, "sha256": evidence._sha256(stripped),
           "before_sha256": before_sha256, "format": "PE-x86_64",
           "strip_log_sha256": evidence._sha256(log),
           "strip_tool_sha256": evidence._sha256(Path(tools["objcopy"])),
           "debug_removal": removal,
           "path": stripped.relative_to(directory).as_posix(),
           "original_path": original.relative_to(directory).as_posix()}
    runner.write_json(directory / f"{label}.json", row)
    return row


def validate_tool_inputs(staging, tools, names=("cargo", "objcopy", "python", "lief_wheel", "lief_init", "lief_native")):
    """Bind these extra Windows tools to the capture's existing input receipt."""
    inputs = json.loads((Path(staging) / "inputs.json").read_text(encoding="utf-8"))
    for name in names:
        path = Path(tools[name])
        recorded = inputs["tools"][name]
        if (recorded != {"path": str(path), "sha256": evidence._sha256(path)}
                or not path.is_file() or path.is_symlink()):
            raise ValueError(f"absolute {name} tool changed")


def _run_build(monitor, command, log, root, env, target):
    """Give each build a new isolated target and retain Cargo's artifact record."""
    if target.exists() or target.is_symlink():
        raise ValueError(f"absolute build target already exists: {target}")
    monitor.run(command, log, root, {**env, "CARGO_TARGET_DIR": str(target)})


def _run_zero_tests(raw, target, root, env, monitor, cargo):
    """Run the two existing zero-allocation cases in their low-debug host."""
    build = raw / "zero-build.jsonl"
    zero_env = {**env, "CARGO_PROFILE_TEST_DEBUG": "0",
                "CARGO_PROFILE_TEST_OPT_LEVEL": "1"}
    _run_build(
        monitor,
        [cargo, "test", "-p", "rca-core", "--test", "composition_zero",
         "--features", "allocation-measurement", "--locked", "-j", "1",
         "--no-run", "--message-format=json"],
        build, root, zero_env, target,
    )
    binary = _artifact(
        build, target, "composition_zero", "test",
        root / "rca-core/tests/composition_qualification/main.rs",
    )
    results = {}
    for label, name in ZERO_TESTS.items():
        log = raw / f"zero-{label}.txt"
        monitor.run(
            [str(binary), name, "--exact", "--nocapture"], log, root, zero_env,
        )
        results[label] = _zero_test(log, name)
    return results


def _run_allocations(raw, target, root, env, monitor, cargo):
    """Run all forty-seven unchanged workload rows, retaining every raw result."""
    build = raw / "allocation-build.jsonl"
    _run_build(
        monitor,
        [cargo, "bench", "-p", "rca-core", "--bench", "composition",
         "--features", "allocation-measurement", "--locked", "-j", "1",
         "--no-run", "--message-format=json"],
        build, root,
        {**env, "CARGO_PROFILE_BENCH_DEBUG": "false", "CARGO_PROFILE_BENCH_LTO": "false",
         "CARGO_PROFILE_BENCH_STRIP": "true"},
        target,
    )
    binary = _artifact(build, target, "composition", "bench",
                       root / "rca-core/benches/composition.rs")
    manifest = evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    lines = []
    for scenario, policy in manifest.items():
        transcript = raw / "allocation" / f"{scenario}.txt"
        monitor.run(
            [str(binary)], transcript, root,
            {**env, "RCA_COMPOSITION_ALLOCATION_SCENARIO": scenario,
             "RCA_COMPOSITION_ALLOCATION_SAMPLES": str(policy["samples"])},
        )
        observed = transcript.read_text(encoding="utf-8").splitlines()
        prefix = (f"scenario={scenario} samples={policy['samples']} "
                  f"logical_operations={policy['logical_operations']} ")
        if len(observed) != 1 or not observed[0].startswith(prefix):
            raise ValueError(f"allocation workload output mismatch: {scenario}")
        lines.extend(observed)
    output = raw / "allocations.txt"
    output.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return allocation_observations(output), binary


def _run_binary_builds(raw, targets, root, env, monitor, tools):
    """Build the four compile fixtures, app test, and Tokio example as PE files."""
    cargo = tools["cargo"]
    fixture_target = targets / "absolute-windows-binary-fixtures"
    fixture_build = raw / "binary-fixture-build.jsonl"
    _run_build(
        monitor,
        [cargo, "build", "--manifest-path", str(root / "benches/compile/Cargo.toml"),
         "--workspace", "--release", "--locked", "-j", "1", "--message-format=json"],
        fixture_build, root, env, fixture_target,
    )
    binaries = {
        name: _artifact(fixture_build, fixture_target, f"rca-compile-{name}", "bin",
                        root / f"benches/compile/{name}/src/main.rs")
        for name in BINARY_NAMES[:4]
    }
    root_target = targets / "absolute-windows-binary-root"
    root_build = raw / "binary-tokio-build.jsonl"
    _run_build(
        monitor,
        [cargo, "build", "-p", "rca-example-composition-search", "--release",
         "--locked", "-j", "1", "--message-format=json"],
        root_build, root, env, root_target,
    )
    binaries["tokio-application"] = _artifact(
        root_build, root_target, "rca-example-composition-search", "bin",
        root / "examples/composition-search/src/main.rs",
    )
    test_build = raw / "binary-app-test-build.jsonl"
    monitor.run(
        [cargo, "test", "-p", "rca-example-composition-search", "--test", "app",
         "--release", "--locked", "-j", "1", "--no-run", "--message-format=json"],
        test_build, root, {**env, "CARGO_TARGET_DIR": str(root_target)},
    )
    binaries["deterministic-composition-test"] = _artifact(
        test_build, root_target, "app", "test",
        root / "examples/composition-search/tests/app.rs",
    )
    return {name: strip_pe(binaries[name], name, raw, env, monitor, tools)
            for name in BINARY_NAMES}


def capture_windows_absolute(capture, protocol, env, monitor, tools):
    """Capture the subject arm only; the caller seals A/A before candidate acceptance."""
    staging = Path(capture["staging"])
    receipt = staging / "sources/subject.json"
    source = runner.validate_staged_source(receipt)
    root = Path(capture["roots"]["subject"])
    if (not evidence.is_windows_protocol(protocol) or Path(source["root"]) != root
            or source["revision"] != capture["subject"]["revision"]):
        raise ValueError("absolute subject source or protocol mismatch")
    receipt_hash = capture["metadata"]["workload_provenance"]["subject"]["receipt_sha256"]
    if evidence._sha256(receipt) != receipt_hash:
        raise ValueError("absolute subject receipt changed")
    if evidence._sha256(root / "Cargo.lock") != capture["subject"]["hashes"]["Cargo.lock"]:
        raise ValueError("absolute subject lock changed")
    validate_tool_inputs(staging, tools)
    evidence.validate_budget_document(runner.BUDGET)
    evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    raw = staging / "absolute-windows-raw"
    raw.mkdir()
    targets = Path(capture["targets"])
    rows, allocation_binary = _run_allocations(
        raw, targets / "absolute-windows-allocation", root, env, monitor, tools["cargo"],
    )
    zero = _run_zero_tests(
        raw, targets / "absolute-windows-zero", root, env, monitor, tools["cargo"],
    )
    binaries = _run_binary_builds(raw, targets, root, env, monitor, tools)
    runner.validate_staged_source(receipt)
    validate_tool_inputs(staging, tools)
    if evidence._sha256(receipt) != receipt_hash:
        raise ValueError("absolute subject receipt changed during capture")
    if evidence._sha256(root / "Cargo.lock") != capture["subject"]["hashes"]["Cargo.lock"]:
        raise ValueError("absolute subject lock changed during capture")
    result = {
        "schema": "composition-windows-absolute-raw-v1",
        "subject_revision": source["revision"],
        "source_receipt_sha256": receipt_hash,
        "root_lock_sha256": evidence._sha256(root / "Cargo.lock"),
        "allocation_manifest_sha256": evidence._sha256(evidence.DEFAULT_ALLOCATION_MANIFEST),
        "allocation_binary_sha256": evidence._sha256(allocation_binary),
        "allocations": rows, "zero_tests": zero, "binaries": binaries,
        "selected": selected_allocation_values(rows, {name: True for name in zero}),
        "input_hashes_after": {
            "source_receipt_sha256": evidence._sha256(receipt),
            "root_lock_sha256": evidence._sha256(root / "Cargo.lock"),
            "allocation_manifest_sha256": evidence._sha256(evidence.DEFAULT_ALLOCATION_MANIFEST),
        },
        "binary_execution_qualified": False,
    }
    runner.write_json(raw / "observation.json", result)
    return result
