"""Capture three existing compiler rejection fixtures with the retained macro DLL.

This bridge calls the capture's guarded command runner. It never builds a crate,
changes a compiler snapshot or turns an expected compiler failure into test success.
The ordinary build and native macro-entry checks remain separate requirements.
"""

from collections import Counter
import hashlib
import json
from pathlib import Path
import re
import shlex


CASES = (
    ("snapshot", "crates/rca-macros/tests/ui/snapshot/fail_syntax.rs", "crates/rca-macros", (
        "expected identifier",
        "snapshot `into` requires a named struct path, not a qualified associated type",
        "expected identifier",
        "expected `const` or `mut`",
        "duplicate snapshot option `redact`",
        "expected snapshot item option `name` or `into`",
        "expected identifier, found reserved identifier `$crate`",
        "Snapshot cannot derive a valid companion name",
    )),
    ("path-names", "crates/rca-macros/tests/ui/state_paths/fail_malformed_path_namespace.rs",
     "crates/rca-macros", (
        "expected identifier, found reserved identifier `$crate`",
        "StatePaths cannot derive a valid path namespace name",
        "expected identifier, found reserved identifier `$crate`",
        "ActionPaths cannot derive a valid path namespace name",
        "expected identifier, found reserved identifier `$crate`",
        "CaseDescriptors cannot derive valid case names",
    )),
    ("missing-dependency", "crates/rca-macros/tests/fixtures/missing-reducers-dependency/src/main.rs",
     "crates/rca-macros/tests/fixtures/missing-reducers-dependency", (
        "RCA macros require `rca`, `rust-composable-architecture` or `rca-core` in Cargo dependencies",
    ) * 5),
)
PROFILE_NAME = re.compile(r"current-([1-9]\d*)-(\d+_\d+)\.profraw")


def digest(path):
    """Hash retained input bytes without reading a dynamic library into memory."""
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def identity(path):
    """Bind the exact absolute compiler or retained library to its bytes."""
    path = Path(path)
    if not path.is_absolute() or not path.is_file() or path.is_symlink():
        raise ValueError("Compiler and retained macro library must be regular absolute files")
    return {"path": str(path), "resolved": str(path.resolve()), "sha256": digest(path)}


def source_identities(root):
    """Bind macro implementation, fixed fixtures and their manifest context."""
    paths = {root / relative for relative in (
        "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "crates/rca-macros/Cargo.toml",
        "crates/rca-macros/tests/fixtures/missing-reducers-dependency/Cargo.toml",
    )}
    paths.update((root / "crates/rca-macros/src").rglob("*.rs"))
    paths.update(root / case[1] for case in CASES)
    if not (root / "crates/rca-macros/src/lib.rs").is_file():
        raise ValueError("Macro implementation is missing")
    if any(not path.is_file() or path.is_symlink() for path in paths):
        raise ValueError("Macro rejection source or manifest is missing or symlinked")
    return {str(path.relative_to(root)): digest(path) for path in sorted(paths)}


def profile_roster(directory):
    """Retain the complete original profile roster, without moving its files."""
    if not directory.is_dir() or directory.is_symlink():
        raise ValueError("Compiler profile directory is missing or symlinked")
    result = {}
    for path in sorted(directory.iterdir()):
        match = PROFILE_NAME.fullmatch(path.name)
        if (not match or not path.is_file() or path.is_symlink()
                or path.stat().st_size == 0):
            raise ValueError("Unexpected, empty or symlinked compiler-stage profile")
        result[path.name] = {"sha256": digest(path), "bytes": path.stat().st_size,
                             "pid": int(match[1]), "module_signature": match[2]}
    if not result:
        raise ValueError("Initial compiler-stage profiles are missing")
    return result


def diagnostic_errors(text, expected):
    """Require the exact real compiler error headlines, including multiplicity."""
    if re.search(r"LLVM Profile (?:Error|Warning)", text):
        raise ValueError("LLVM profiling diagnostics invalidate compiler rejection evidence")
    messages = []
    for line in text.splitlines():
        try:
            diagnostic = json.loads(line)
        except (ValueError, TypeError) as error:
            raise ValueError("Compiler rejection output is not JSON diagnostics") from error
        if not isinstance(diagnostic, dict) or diagnostic.get("$message_type") != "diagnostic":
            raise ValueError("Unexpected compiler diagnostic record")
        if diagnostic.get("level") == "error":
            messages.append(diagnostic.get("message"))
    summary = f"aborting due to {len(expected)} previous errors"
    if messages.count(summary) != 1:
        raise ValueError("Compiler rejection lacks its exact error-count summary")
    messages.remove(summary)
    if Counter(messages) != Counter(expected):
        raise ValueError("Compiler rejection diagnostics differ from the fixed fixture")
    return messages


def rejection_command(compiler, env, root, retained, dependency_dir, directory, case):
    """Build one fixed rejection command from the captured compiler configuration."""
    name, source, _, _ = case
    return [compiler, *shlex.split(env["RUSTFLAGS"]), "--edition=2024", "--emit=metadata",
            "--error-format=json", "--crate-name=rca_rejection_" + name.replace("-", "_"),
            "--extern", "rca_macros=" + str(retained),
            "-L", "dependency=" + str(dependency_dir),
            "--out-dir", str(directory), str(root / source)]


def validate_invocation(record, directory, case):
    """Reject stopped commands, unrelated errors and missing fresh module profiles."""
    name, _, _, expected = case
    prefix = directory / name
    for suffix, expected_hash in record["output_sha256"].items():
        path = prefix.with_suffix(suffix)
        if not path.is_file() or path.is_symlink() or digest(path) != expected_hash:
            raise ValueError("Retained compiler command evidence changed")
    status = json.loads(prefix.with_suffix(".status.json").read_text())
    if status != record["status"] or status.get("returncode") != 1 or status.get("stop_reason") is not None:
        raise ValueError("Compiler rejection did not exit naturally with status 1")
    if json.loads(prefix.with_suffix(".argv.json").read_text()) != record["command"]:
        raise ValueError("Guarded compiler command differs from its retained arguments")
    if prefix.with_suffix(".stdout").read_text():
        raise ValueError("Unexpected compiler rejection stdout")
    diagnostic_errors(prefix.with_suffix(".stderr").read_text(), expected)
    before, after = record["profiles_before"], record["profiles_after"]
    if any(after.get(name) != value for name, value in before.items()):
        raise ValueError("An existing compiler-stage profile changed")
    added = set(after) - set(before)
    if len(added) != 1:
        raise ValueError("Expected one fresh profile from the retained macro DLL")
    added_name = next(iter(added))
    module = after[added_name]["module_signature"]
    if module not in {value["module_signature"] for value in before.values()}:
        raise ValueError("Fresh compiler profile has no initial-build module identity")
    if record["new_profile"] != added_name or record["module_signature"] != module:
        raise ValueError("Compiler rejection profile identity differs from its receipt")


def validate_macro_rejections(root, macro, retained, expected_env, expected_rustc_identity):
    """Recheck retained rejection evidence for capture or external admission.

    The admission caller supplies the original allowlisted environment and pinned
    compiler identity from the capture's independently checked tool records.
    Compiler paths remain historical identities when an artefact moves hosts.
    """
    root, macro, retained = Path(root), Path(macro), Path(retained)
    directory = macro / "rejections"
    receipt = json.loads((directory / "receipt.json").read_text())
    if (receipt.get("schema") != 1 or receipt.get("capture_complete") is not True
            or receipt.get("environment") != expected_env
            or receipt.get("rustc_before") != expected_rustc_identity
            or receipt.get("rustc_after") != expected_rustc_identity
            or receipt.get("source_before") != source_identities(root)
            or receipt.get("source_after") != receipt["source_before"]
            or receipt.get("dll_before", {}).get("sha256") != digest(retained)
            or receipt.get("dll_after") != receipt["dll_before"]
            or len(receipt.get("invocations", [])) != len(CASES)):
        raise ValueError("Compiler rejection inputs or configuration changed")
    previous = receipt["profiles_before"]
    module = None
    original_root = Path(receipt["root"])
    original_directory = Path(receipt["directory"])
    for record, case in zip(receipt["invocations"], CASES):
        name, source, manifest, _ = case
        expected_command = rejection_command(
            expected_rustc_identity["path"], expected_env, original_root,
            receipt["dll_before"]["path"], receipt["dependency_dir"], original_directory, case,
        )
        if (record.get("case") != name or record.get("source") != source
                or record.get("manifest") != manifest or record["profiles_before"] != previous
                or record.get("command") != expected_command
                or record.get("cwd") != str(original_root / manifest)
                or record.get("timeout_seconds") != 120
                or record.get("environment") != dict(
                    expected_env, CARGO_MANIFEST_DIR=str(original_root / manifest))):
            raise ValueError("Compiler rejection fixtures or profile chain differ")
        validate_invocation(record, directory, case)
        if module is not None and record["module_signature"] != module:
            raise ValueError("Compiler rejection invocations loaded different modules")
        module = record["module_signature"]
        previous = record["profiles_after"]
    if previous != receipt["profiles_after"] or profile_roster(macro.parent / "build-raw") != previous:
        raise ValueError("Final compiler rejection profile roster changed")
    return True


def capture_macro_rejections(root, macro, retained, dependency_dir, tools, env, run_command):
    """Run only the three fixed rustc fixtures through the existing guarded runner."""
    root, macro = Path(root), Path(macro)
    retained, dependency_dir = Path(retained), Path(dependency_dir)
    directory = macro / "rejections"
    if directory.exists():
        raise ValueError("Compiler rejection evidence already exists")
    directory.mkdir()
    raw = macro.parent / "build-raw"
    receipt = {"schema": 1, "capture_complete": False, "environment": dict(env),
               "root": str(root), "directory": str(directory),
               "dependency_dir": str(dependency_dir), "invocations": []}
    try:
        compiler = identity(tools["rustc"])
        receipt.update(rustc_before=compiler, dll_before=identity(retained),
                       source_before=source_identities(root), profiles_before=profile_roster(raw))
        flags = shlex.split(env.get("RUSTFLAGS", ""))
        if (env.get("RUSTC") != compiler["path"] or env.get("RUSTC_BOOTSTRAP") != "1"
                or env.get("LLVM_PROFILE_FILE") != str(raw / "current-%p-%m.profraw")
                or "instrument-coverage" not in flags or "coverage-options=branch" not in flags
                or not dependency_dir.is_absolute() or not dependency_dir.is_dir()
                or dependency_dir.is_symlink() or retained.suffix != ".so"):
            raise ValueError("Compiler rejection inputs do not match the instrumented build")
        previous = receipt["profiles_before"]
        for case in CASES:
            name, source, manifest, _ = case
            prefix = directory / name
            invocation_env = dict(env, CARGO_MANIFEST_DIR=str(root / manifest))
            command = rejection_command(
                compiler["path"], env, root, retained, dependency_dir, directory, case,
            )
            record = {"case": name, "source": source, "manifest": manifest,
                      "command": command, "environment": invocation_env,
                      "cwd": str(root / manifest), "timeout_seconds": 120,
                      "profiles_before": previous}
            receipt["invocations"].append(record)
            try:
                run_command(command, root / manifest, invocation_env, prefix, 120)
            except ValueError as error:
                if str(error) != f"Command failed; see {prefix}.status.json":
                    raise
            else:
                raise ValueError("Guarded compiler did not report its expected failure")
            record["status"] = json.loads(prefix.with_suffix(".status.json").read_text())
            record["output_sha256"] = {suffix: digest(prefix.with_suffix(suffix)) for suffix in (
                ".argv.json", ".status.json", ".stdout", ".stderr")}
            after = profile_roster(raw)
            record["profiles_after"] = after
            added = set(after) - set(previous)
            if len(added) != 1:
                raise ValueError("Expected one fresh profile from the retained macro DLL")
            record["new_profile"] = next(iter(added))
            record["module_signature"] = after[record["new_profile"]]["module_signature"]
            validate_invocation(record, directory, case)
            if (identity(tools["rustc"]) != compiler or identity(retained) != receipt["dll_before"]
                    or source_identities(root) != receipt["source_before"] or dict(env) != receipt["environment"]):
                raise ValueError("Compiler rejection source, DLL, tool or configuration changed")
            previous = after
        receipt.update(rustc_after=identity(tools["rustc"]), dll_after=identity(retained),
                       source_after=source_identities(root), profiles_after=profile_roster(raw),
                       capture_complete=True)
        (directory / "receipt.json").write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
        validate_macro_rejections(root, macro, retained, env, compiler)
    except BaseException as error:
        receipt.update(capture_complete=False, failure=f"{type(error).__name__}: {error}")
        raise
    finally:
        (directory / "receipt.json").write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
    return receipt
