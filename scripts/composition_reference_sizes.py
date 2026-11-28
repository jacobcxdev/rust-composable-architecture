"""Enrol six original reference PE files; measure the candidate from source in the current run."""

import hashlib
import json
from pathlib import Path, PureWindowsPath
import shutil
import subprocess
import tarfile
import tempfile

import composition_performance as evidence
from composition_product_commands import product_commands
import composition_runner as runner
import composition_timing_inputs as timing
import composition_windows_absolute as absolute
import composition_windows_complete as complete
import release_baseline


MODE = "reference-reference-aa"
NAMES = absolute.BINARY_NAMES


def _digest(data):
    return hashlib.sha256(json.dumps(data, sort_keys=True, separators=(",", ":"),
                                    allow_nan=False).encode()).hexdigest()


def _read(path):
    return json.loads(path.read_bytes())


def _reference_inputs(source, meta, tools):
    return {"tree": source["tree"], "effective_source_sha256": _digest(source["effective"]),
            "lock_sha256": source["effective"]["Cargo.lock"], "workload_sha256": meta["workload_sha256"],
            "toolchain_sha256": _digest([meta["toolchain"], tools]),
            "configuration_sha256": _digest([meta["protocol_sha256"], meta["budget_sha256"],
                 source["effective"]["Cargo.toml"], source["effective"]["benches/compile/Cargo.toml"]]),
            "platform_sha256": _digest([meta[k] for k in ("platform", "target", "os_version", "os_build", "runner")])}


def _extract_reference(archive, pin, directory):
    """Read only fixed locations in an externally authenticated numerical archive."""
    prefix = f"composition-v3/{MODE}/"
    if not evidence._HASH.fullmatch(pin) or archive.resolve() != archive:
        raise ValueError("expected a direct, externally pinned archive")
    with archive.open("rb") as stream:
        if hashlib.file_digest(stream, "sha256").hexdigest() != pin:
            raise ValueError("reference archive differs from the independent enrolment pin")
        stream.seek(0)
        with tarfile.open(fileobj=stream, mode="r:gz") as bundle:
            for member in bundle:
                key = member.name
                if not (key.startswith(prefix) or key in ("composition-v3/capture.json", "composition-v3/telemetry/processes.jsonl")):
                    continue
                if member.isdir():
                    continue
                name = key.removeprefix("composition-v3/")
                output = directory / name
                if (not member.isfile() or ".." in Path(name).parts or "\\" in name or ":" in name
                        or not output.resolve().is_relative_to(directory)):
                    raise ValueError("non-regular or redirected reference archive member")
                output.parent.mkdir(parents=True, exist_ok=True)
                with bundle.extractfile(member) as src, output.open("xb") as dest:
                    shutil.copyfileobj(src, dest)


def _prove(directory):
    arm = directory / MODE
    raw = arm / "absolute-windows-raw"
    observation = evidence._sha256(raw / "observation.json")
    meta, observed = complete._read_experimental_mode(directory, MODE, observation)
    policy = complete.load_policy(complete.POLICY)
    complete._experimental_product_input(policy, MODE, meta, observed)
    complete._product_observations(evidence.validate_budget_document(runner.BUDGET), meta, observed)
    inputs, source = _read(arm / "inputs.json"), _read(arm / "sources/subject.json")
    reference = release_baseline.require_baseline()
    if (any(meta[key] != reference.revision for key in ("subject_revision", "control_revision"))
            or inputs["subject"]["tree"] != inputs["control"]["tree"] or meta["target"] != "x86_64-pc-windows-msvc"):
        raise ValueError("only the frozen independent Windows reference may be enrolled")
    subprocess.run(["git", "-C", str(runner.ROOT), "verify-commit", reference.revision], capture_output=True, check=True)
    if source["tree"] != reference.tree:
        raise ValueError("reference product sources differ from the signed reference tree")
    protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
    with tempfile.TemporaryDirectory(dir=directory) as temporary:
        stage = runner.stage_source(runner.ROOT, {"revision": reference.revision, "tree": reference.tree},
                                    Path(temporary)/"source", protocol)
        if any(stage[k] != source[k] for k in ("original", "effective", "adapters", "common_workloads")):
            raise ValueError("reference source or consumer differs from the signed reconstruction")
    events = [json.loads(line) for line in (directory/"telemetry/processes.jsonl").read_bytes().splitlines()]
    suffix = f"{MODE}/absolute-windows-raw/binary-fixture-build.jsonl"
    starts = [PureWindowsPath(e["destination"]) for e in events if e.get("phase") == "start"
              and PureWindowsPath(e.get("destination", "")).as_posix().endswith("/" + suffix)]
    if len(starts) != 1:
        raise ValueError("missing unique reference build")
    capture_root = starts[0].parents[2]
    commands = product_commands(directory, MODE, source["root"], inputs["tools"],
                                timing._executable(arm/"builds/subject.txt"), capture_root)
    commands = [spec for spec in commands if Path(spec[0]).name.startswith(("binary-", "strip-", "debug-strip-"))]
    files = {capture_root / s[0] for s in commands}
    pairs = [e for e in events if PureWindowsPath(e.get("destination", "")) in files]
    if len(commands) != 15 or len(pairs) != 30:
        raise ValueError("expected three reference builds and twelve stripping jobs")
    last = -1
    for spec, first, end in zip(commands, pairs[::2], pairs[1::2]):
        if not last <= first["monotonic"] < end["monotonic"]:
            raise ValueError("reference build and strip lifetimes are not ordered")
        timing._terminal_pair(directory, protocol, capture_root, spec, first, end, {},
                              telemetry=inputs["tools"]["compiler_telemetry"])
        last = end["monotonic"]
    key = _reference_inputs(source, meta, inputs["tools"])
    provenance = {"revision": reference.revision, "tree": reference.tree, "observation_sha256": observation,
                  "source_receipt_sha256": evidence._sha256(arm/"sources/subject.json"),
                  "commands_sha256": _digest(pairs), "inputs_sha256": evidence._sha256(arm/"inputs.json")}
    root = PureWindowsPath(source["root"])
    return key, provenance, str(root), str(root.parent / "absolute-windows-binary"), observed["binaries"]


def enrol(cache, *, archive, archive_sha256):
    """Create one external B-only enrolment. Trust the returned digest separately from the cache."""
    cache, archive = Path(cache), Path(archive)
    if (cache.exists() or cache.resolve() != cache or not cache.parent.is_dir()
            or subprocess.run(["git", "-C", str(cache.parent), "rev-parse", "--git-dir"],
                              capture_output=True).returncode == 0):
        raise ValueError("reference products need a new external cache outside Git")
    with tempfile.TemporaryDirectory(dir=cache.parent) as temporary:
        scratch = Path(temporary)
        _extract_reference(archive, archive_sha256, scratch)
        inputs, provenance, source, target, binaries = _prove(scratch)
        provenance["archive_sha256"] = archive_sha256
        bundle = scratch / "enrolled"
        role = bundle / "b"
        role.mkdir(parents=True)
        products = {}
        for name in NAMES:
            path = role / (name + ".exe")
            shutil.copyfile(scratch / MODE / "absolute-windows-raw" / binaries[name]["path"], path)
            products[name] = evidence._sha256(path)
            if products[name] != binaries[name]["sha256"] or absolute._pe_identity(path) != binaries[name]["bytes"]:
                raise ValueError("reference image changed during enrolment")
        runner.write_json(role / "provenance.json", provenance)
        role_record = {"source_root": source, "target_root": target, "products": products,
                       "provenance_sha256": evidence._sha256(role / "provenance.json")}
        manifest = {"schema": 1, "purpose": "composition-pe", "revision": provenance["revision"], "inputs": inputs,
                    "roles": {"b": role_record}}
        runner.write_json(bundle / "enrolment.json", manifest)
        digest = evidence._sha256(bundle / "enrolment.json")
        cache.mkdir()
        # A reader accepts nothing until the manifest has arrived.
        role.rename(cache / "b")
        (bundle / "enrolment.json").rename(cache / "enrolment.json")
    return digest, inputs


def reference_sizes(cache, *, enrolment_sha256, inputs):
    """Read and size the six verified reference images. No timing or candidate measurement is cached."""
    from qualification_reference_cache import verify_reference_products
    images = verify_reference_products(cache, enrolment_sha256=enrolment_sha256, inputs=inputs,
                                       purpose="composition-pe", names=NAMES, roles=("b",))["b"]
    manifest = complete._retained(Path(cache), "enrolment.json").read_bytes()
    if hashlib.sha256(manifest).hexdigest() != enrolment_sha256:
        raise ValueError("reference enrolment changed during sizing")
    expected = json.loads(manifest)["roles"]["b"]["products"]
    sizes = {}
    for name, path in images.items():
        # Size the very bytes whose stripped PE format we checked during enrolment.
        image = path.read_bytes()
        if hashlib.sha256(image).hexdigest() != expected[name]:
            raise ValueError("reference image changed during sizing")
        sizes[name] = len(image)
    return sizes


def compare_fresh(capture, env, monitor, tools, *, cache, enrolment_sha256, candidate_revision, candidate_tree):
    """Build the current candidate and compare six PE sizes. Never read cached candidate data."""
    import capture_windows_support as support
    meta = capture["metadata"]
    stage, targets = Path(capture["staging"]), Path(capture["targets"])
    root = Path(capture["roots"]["subject"])
    if (capture["mode"] != "candidate-reference" or meta["mode"] != capture["mode"] or stage.name != capture["mode"]
            or targets.name != capture["mode"] or root != targets / "source-subject"
            or meta["control_revision"] != release_baseline.require_baseline().revision
            or meta["subject_revision"] == meta["control_revision"]
            or (meta["subject_role"], meta["control_role"]) != ("candidate", "reference")
            or meta["subject_revision"] != candidate_revision or meta["measured_subject_revision"] != candidate_revision
            or meta["method_version"] != evidence.WINDOWS_V6_METHOD
            or meta["protocol_sha256"] != evidence._sha256(runner.WINDOWS_PROTOCOL)
            or meta["budget_sha256"] != evidence._sha256(runner.BUDGET)):
        raise ValueError("expected a fresh candidate capture with the frozen Windows policy")
    active = _read(complete._retained(stage, "inputs.json"))
    if (active["mode"] != capture["mode"] or active["measured_subject"]["revision"] != candidate_revision
            or active["measured_subject"]["tree"] != candidate_tree):
        raise ValueError("capture does not name the enrolled candidate")
    sources = {}
    for role in ("control", "subject"):
        receipt = complete._retained(stage, f"sources/{role}.json")
        expected = meta["workload_provenance"][role]["receipt_sha256"]
        if evidence._sha256(receipt) != expected:
            raise ValueError(f"{role} source receipt changed")
        source = runner.validate_staged_source(receipt)
        if (source["revision"] != meta[f"{role}_revision"] or source["revision"] != active[role]["revision"]
                or source["tree"] != active[role]["tree"] or (role == "subject" and source["tree"] != candidate_tree)
                or Path(source["root"]) != targets / f"source-{role}" or capture[role] != active[role]
                or source["effective"]["Cargo.lock"] != meta[f"{role}_lock_sha256"]
                or any(source["original"].get(k) != digest for k, digest in active[role]["hashes"].items())):
            raise ValueError(f"{role} source does not belong to this capture")
        sources[role] = source
    current = active["tools"]
    inputs = _reference_inputs(sources["control"], meta, current)
    absolute.validate_tool_inputs(stage, tools, names=("rustc", "rustdoc", "cargo", "objcopy", "python",
                                                      "lief_wheel", "lief_init", "lief_native"))
    bound = reference_sizes(cache, enrolment_sha256=enrolment_sha256, inputs=inputs)
    raw = stage / "absolute-windows-raw"
    if raw.resolve() != raw:
        raise ValueError("redirected PE evidence")
    raw.mkdir(exist_ok=True)
    clean_env = support.build_environment({k:v for k,v in env.items() if not k.upper().startswith("CARGO_PROFILE_BENCH_")},
                                          current, targets / "absolute-windows-binary")
    binaries = absolute._run_binary_builds(raw, targets, root, clean_env, monitor, tools)
    if evidence._sha256(receipt) != expected or runner.validate_staged_source(receipt) != sources["subject"]:
        raise ValueError("candidate source changed during sizing")
    return _compare(bound, binaries)


def _compare(reference, binaries):
    """Use the same byte ceiling and rounding as the existing release gate."""
    policy = complete.load_policy(complete.POLICY)
    budget = evidence.validate_budget_document(runner.BUDGET)
    if set(reference) != set(NAMES) or set(binaries) != set(NAMES):
        raise ValueError("expected all six reference and candidate binaries")
    rows = [r for r in budget["rows"] if r["scenario"].startswith("binary/")]
    def values(sizes):
        if any(type(v) is not int or v <= 0 for v in sizes.values()):
            raise ValueError("expected positive measured PE sizes")
        return {(f"binary/{n}", "stripped_file_size"): v for n, v in sizes.items()}
    limits = complete._reference_rows(policy, rows, values(reference))
    actual = values({k:v["bytes"] for k,v in binaries.items()})
    checks = [complete._result(row, actual) for row in limits]
    return {"scope": "six-windows-PE-size-gates", "checks": checks, "passed": all(row["passed"] for row in checks)}
