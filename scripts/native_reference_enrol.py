"""Enrol two independently compiled stock Windows reference families outside Git."""

import hashlib
import io
import json
from pathlib import Path
import shutil
import tempfile
import tarfile

from capture_windows_support import provider_module as native
from composition_registry_seed import _direct, _regular, _sha
from native_reference_stock import PRODUCTS, WORKLOAD, inputs
from qualification_reference_cache import _git_root, verify_reference_products
from release_baseline import git, require_baseline


PERMITTED_ENV = {
    "Path",
    "RUSTC",
    "RUSTDOC",
    "RUSTUP_TOOLCHAIN",
    "CARGO_INCREMENTAL",
    "CARGO_BUILD_JOBS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
}


def _build_source(row, expected, *, normal):
    if row["source_files"] != expected or (normal and row["core_artifact"]["fresh"] is not False):
        raise ValueError("reference did not compile its own verified source")


def _process(commands, assembled):
    matches = [c for c in commands if c.get("log_sha256") == assembled["transcript_sha256"]]
    fields = (
        "command",
        "log",
        "log_sha256",
        "process",
        "host_guard",
        "aggregate_job",
        "started_at",
        "finished_at",
    )
    if len(matches) != 1 or any(matches[0].get(k) != assembled[k] for k in fields):
        raise ValueError("reference command or outcome differs from the original build")


def _read(path, pin):
    data = _regular(path).read_bytes()
    if hashlib.sha256(data).hexdigest() != pin:
        raise ValueError(f"reference receipt differs from its independent digest: {path}")
    return json.loads(data)


def _independent(a, b):
    for key in ("sources", "targets", "images", "transcripts"):
        if a[key] & b[key]:
            raise ValueError(f"reference builds share {key}")


def release_files(revision):
    """Check the same Git archive bytes that supply a clean reference build."""
    files = {}
    with tarfile.open(fileobj=io.BytesIO(git("archive", "--format=tar", revision)), mode="r:") as archive:
        for member in archive:
            if member.isdir():
                continue
            if not member.isfile() or member.name in files:
                raise ValueError("unsupported or duplicate source entry in the release")
            with archive.extractfile(member) as stream:
                files[member.name] = hashlib.file_digest(stream, "sha256").hexdigest()
    return files


def _family(metadata, pin, capture, capture_pin, host, release_source):
    meta = _read(metadata, pin)
    observed = _read(capture, capture_pin)["metadata"]
    if observed["source_build_sha256"] != pin or any(observed.get(k) != v for k,v in host.items()):
        raise ValueError("original reference measurement used another build or Windows host")
    native.validate_metadata(meta)
    if (
        meta.get("git_source_verification") != {side: meta[side] for side in ("reference", "candidate")}
        or meta["build_exit_code"] != 0
        or meta["profile_environment"]
    ):
        raise ValueError("expected a Git-verified stock build with no profile overrides")
    native.validate_build_transcripts(meta)
    native.validate_staged_sources(meta)
    paths = [(Path(k), h) for k, h in meta["input_hashes"].items() if Path(k).name == "native-build.json"]
    if len(paths) != 1:
        raise ValueError("expected exactly one hash-bound original build record")
    path, digest = paths[0]
    build = _read(path, digest)
    original = build["source_roles"]["reference"]["original_files"]
    if (
        original != release_source
        or build["complete"] is not True
        or build["measurement_toolchain"] != "1.100.0"
        or build["common_workloads"] != {p: meta[k] for k, p in WORKLOAD.items()}
    ):
        raise ValueError("unexpected original source, toolchain or common workload")
    inventories = {}
    for key in ("reference_normal", "reference_deterministic_normal"):
        b = meta["binaries"][key]
        inventories[b["configuration"]] = _read(Path(b["source_manifest"]), b["source_manifest_sha256"])
    if inventories["deterministic"] != {
        **original,
        **build["common_workloads"],
    }:
        raise ValueError("deterministic reference differs from the signed baseline plus the stock workload")
    tools = {
        name: {key: build["tools"][name + ".exe"][key] for key in ("path", "sha256")}
        for name in ("cargo", "rustc", "rustdoc")
    }
    binding = inputs(meta, tools, host)
    env = build["environment"]
    if (
        (env["RUSTC"], env["RUSTDOC"]) != (tools["rustc"]["path"], tools["rustdoc"]["path"])
        or (env["RUSTUP_TOOLCHAIN"], env["CARGO_INCREMENTAL"], env["CARGO_BUILD_JOBS"])
        != ("1.100.0", "0", "1")
        or not env.keys() <= PERMITTED_ENV
    ):
        raise ValueError("wrong reference compiler or build profile")
    origins = {k: set() for k in ("sources", "targets", "images", "transcripts")}
    items = {}
    commands = {c["binary_role"]: c for c in meta["commands"]}
    for name in PRODUCTS:
        key = "reference_" + name.removeprefix("native-").replace("-", "_")
        b, c = meta["binaries"][key], commands[key]
        if observed["binaries"][key]["sha256"] != b["sha256"]:
            raise ValueError("original measured reference image differs from its build")
        row = build["builds"]["reference-" + name]
        _process(build["commands"], c)
        _build_source(row, inventories[b["configuration"]], normal=name.endswith("-normal"))
        source = _direct(Path(b["source_root"]))
        target = _direct(Path(row.get("target_dir") or env["CARGO_TARGET_DIR"]))
        image = Path(row["binary"])
        image = _regular(image if image.is_file() else Path(b["path"]))
        log = _regular(Path(c["transcript"]))
        if (
            row["source_root"] != str(source)
            or row["build_log_sha256"] != c["transcript_sha256"]
            or row["binary_sha256"] != b["sha256"]
            or _sha(image) != b["sha256"]
            or row["core_artifact"] != c["core_artifact"]
            or row["benchmark_artifact"] != c["cargo_artifact"]
            or c["cargo_artifact"]["fresh"] is not False
            or not Path(b["path"]).is_relative_to(target)
            or c["aggregate_job"]["active_processes"] != 0
            or c["host_guard"]["failure"] is not None
            or c["process"]["descendants_exited"] is not True
        ):
            raise ValueError(f"reference product lacks a successful compiled benchmark of its own: {name}")
        for group, v in (("sources", source), ("targets", target), ("images", image), ("transcripts", log)):
            origins[group].add(v)
        items[name] = {
            "source": str(source),
            "target": str(target),
            "image": str(image),
            "sha256": b["sha256"],
            "transcript": str(log),
            "transcript_sha256": c["transcript_sha256"],
        }
    if len(origins["sources"]) != 2 or len(origins["images"]) != 4 or len(origins["transcripts"]) != 4:
        raise ValueError("expected two distinct configurations and four separate product builds")
    return (
        binding,
        origins,
        {
            "metadata": str(metadata),
            "metadata_sha256": pin,
            "capture_sha256": capture_pin,
            "build": str(path),
            "build_sha256": digest,
            "tool_installation_sha256": build["tool_installation_sha256"],
            "products": items,
        },
    )


def enrol(cache, a, b):
    """Each family supplies the build path, its digest, the capture input path and its digest."""
    cache = _direct(Path(cache))
    if cache.exists() or not cache.parent.is_dir() or any(_git_root(p) for p in (cache, *cache.parents)):
        raise ValueError("native reference cache must be new, local and outside Git")
    host = native.host_metadata()
    release = require_baseline()
    release_source = release_files(release.revision)
    with tempfile.TemporaryDirectory(dir=cache.parent) as temporary:
        staging = Path(temporary) / "stock"
        staging.mkdir()
        roles = {}
        prior = expected = first = None
        for role, source in (("a", a), ("b", b)):
            binding, origins, proof = _family(Path(source[0]), source[1], Path(source[2]), source[3], host, release_source)
            if prior is not None:
                if (
                    expected != binding
                    or first["tool_installation_sha256"] != proof["tool_installation_sha256"]
                ):
                    raise ValueError("reference builds differ in their effective inputs or toolchain")
                _independent(prior, origins)
            else:
                prior, expected, first = origins, binding, proof
            home = staging / role
            home.mkdir()
            products = {}
            for name, item in proof["products"].items():
                out = home / (name + ".exe")
                shutil.copyfile(item["image"], out)
                if _sha(out) != item["sha256"]:
                    raise ValueError("reference image changed during enrolment")
                products[name] = item["sha256"]
            native.write_json(home / "provenance.json", proof)
            anchor = proof["products"]["native-normal"]
            roles[role] = dict(
                source_root=anchor["source"],
                target_root=anchor["target"],
                products=products,
                provenance_sha256=_sha(home / "provenance.json"),
            )
        native.write_json(
            staging / "enrolment.json",
            dict(
                schema=1,
                revision=release.revision,
                inputs=expected,
                purpose="native-stock",
                roles=roles,
            ),
        )
        pin = _sha(staging / "enrolment.json")
        verify_reference_products(
            staging, enrolment_sha256=pin, inputs=expected, purpose="native-stock", names=PRODUCTS
        )
        staging.rename(cache)
    return pin, expected
