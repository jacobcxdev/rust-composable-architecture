"""Reconstruct the expected Windows product jobs from frozen workloads and real builds.

Windows paths here identify original commands. Only fixed, capture-relative local
files are opened by the offline verifier; no recorded executable is run.
"""

import json
from pathlib import Path, PureWindowsPath

import composition_performance as evidence
import composition_windows_absolute as absolute


def _read(root, rel):
    path = root / rel
    if path.resolve() != path or not path.is_file():
        raise ValueError(f"missing or redirected product command input: {rel}")
    return path.read_text(encoding="utf-8")


def _built_host(log, name, kind, source, target):
    found = []
    for line in log.splitlines():
        if not line.startswith("{"):
            continue
        row = json.loads(line)
        unit = row.get("target", {})
        if (row.get("reason") != "compiler-artifact" or unit.get("name") != name
                or not isinstance(unit.get("kind"), list) or kind not in unit["kind"]
                or not row.get("executable")):
            continue
        exe = PureWindowsPath(row["executable"])
        if (row.get("fresh") is not False or not isinstance(row.get("features"), list)
                or "allocation-measurement" not in row["features"]
                or PureWindowsPath(unit["src_path"]) != source
                or not exe.is_absolute() or ".." in exe.parts or not exe.is_relative_to(target)
                or exe.suffix.lower() != ".exe"):
            raise ValueError(f"wrong source, feature, target or freshness for {name}")
        found.append(exe)
    if len(found) != 1:
        raise ValueError(f"expected exactly one freshly built {name} host")
    return found[0]


def product_commands(directory, mode, source_root, tools, executable, capture_root):
    """List (relative log, argv, original cwd, command kind, Cargo target) in strict order."""
    directory = Path(directory).resolve(strict=True)
    root, stage = PureWindowsPath(source_root), PureWindowsPath(capture_root)
    if (mode not in ("reference-reference-aa", "candidate-reference") or not stage.is_absolute()
            or ".." in root.parts or ".." in stage.parts
            or not root.is_absolute() or root.name != "source-subject" or root.parent.name != mode):
        raise ValueError("product commands require the captured Windows staging and subject roots")
    prefix = f"{mode}/absolute-windows-raw"
    raw = stage / prefix
    controller = PureWindowsPath(json.loads(_read(directory, "capture.json"))["controller"]["root"])
    if not controller.is_absolute() or ".." in controller.parts:
        raise ValueError("missing absolute controller identity for the stripping helper")
    cargo = tools["cargo"]["path"]
    subject = root.parent / "subject"
    zero_target = root.parent / "absolute-windows-zero"
    binary_target = root.parent / "absolute-windows-binary"
    allocation = _built_host(_read(directory, prefix+"/allocation-build.jsonl"), "composition", "bench",
                             root/"rca-core/benches/composition.rs", subject)
    zero = _built_host(_read(directory, prefix+"/zero-build.jsonl"), "composition_zero", "test",
                       root/"rca-core/tests/composition_qualification/main.rs", zero_target)
    if allocation == PureWindowsPath(executable):
        raise ValueError("an ordinary timing executable cannot provide allocation evidence")
    jobs = []

    def add(log, command, *, cwd=root, target=None):
        jobs.append((prefix + "/" + log, list(map(str, command)), cwd,
                     "runtime" if target is None else "compile", target))

    measured = ["--features", "allocation-measurement", "--locked", "-j", "1", "--no-run", "--message-format=json"]
    add("allocation-build.jsonl", [cargo, "bench", "-p", "rca-core", "--bench", "composition", *measured],
        target=subject)
    for name in evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST):
        add(f"allocation/{name}.txt", [allocation])
    add("zero-build.jsonl", [cargo, "test", "-p", "rca-core", "--test", "composition_zero", *measured],
        target=zero_target)
    for label, name in absolute.ZERO_TESTS.items():
        add(f"zero-{label}.txt", [zero, name, "--exact", "--nocapture"])

    def strip(label):
        base = raw/"binaries"
        intermediate = base/(label+"-llvm-stripped.exe")
        add(f"strip-{label}.txt", [tools["objcopy"]["path"], "--strip-all", base/(label+"-original.exe"),
                                  intermediate], cwd=raw)
        add(f"debug-strip-{label}.txt", [tools["python"]["path"], "-I", "-S", controller/"scripts/windows_strip.py",
                 "--wheel", tools["lief_wheel"]["path"], "--site", PureWindowsPath(tools["lief_init"]["path"]).parent.parent,
                 "--input", intermediate, "--output", base/(label+"-stripped.exe")], cwd=raw)

    release = ["--release", "--locked", "-j", "1"]
    add("binary-fixture-build.jsonl", [cargo, "build", "--manifest-path", root/"benches/compile/Cargo.toml",
                                      "--workspace", *release, "--message-format=json"], target=binary_target)
    for name in absolute.BINARY_NAMES[:4]:
        strip(name)
    add("binary-tokio-build.jsonl", [cargo, "build", "-p", "rca-example-composition-search", *release,
                                     "--message-format=json"], target=binary_target)
    # Cargo's test invocation may overwrite the normal binary. The retained
    # normal image must have been stripped before that next process starts.
    strip("tokio-application")
    add("binary-app-test-build.jsonl", [cargo, "test", "-p", "rca-example-composition-search", "--test", "app",
                                       *release, "--no-run", "--message-format=json"], target=binary_target)
    strip("deterministic-composition-test")
    return jobs
