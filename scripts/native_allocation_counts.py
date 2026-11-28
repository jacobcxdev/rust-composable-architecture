"""Prepare or check a native allocation counter without changing product source."""

import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import tarfile
import tomllib

from composition_registry_seed import _records, _regular, _sha as digest, REGISTRY


CRATE = "dhat-0.3.3"
ARCHIVE = "98cd11d84628e233de0ce467de10b8633f4ddaecafadefc86e13b84b8739b827"
STOCK = "a9eda549ebabbb4f114efe194d280d02a08a817204c0836eacec7280efb3abab"
PATCHED = "887a442969aa20d6c68629ecea22c09c40498058b4866a3b9f993fbef63e341f"
EMPTY_STACK = b"""macro_rules! new_backtrace {
    ($g:expr) => {{
        Backtrace(backtrace::Backtrace::from(Vec::<backtrace::BacktraceFrame>::new()))
    }};
}
"""
CONFIGURATIONS = ("native", "deterministic")


def files(directory):
    """Hash plain files only. Reject links, including linked directories."""
    hashes = {}
    for path in directory.rglob("*"):
        if path.is_symlink() or path.resolve(strict=True) != path:
            raise ValueError(f"linked source input: {path}")
        if not path.is_dir():
            hashes[path.relative_to(directory).as_posix()] = digest(_regular(path))
    return hashes


def tree_digest(directory):
    encoded = json.dumps(files(directory), sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def core_features(source, configuration):
    """Read the features that the checked source enables for the deterministic runner."""
    if configuration == "native":
        return ["default"]
    manifest = tomllib.loads((source / "crates/rca-test/Cargo.toml").read_text(encoding="utf-8"))
    dependency = manifest["dependencies"]["rca-core"]
    if dependency not in (
        {"workspace": True, "features": ["test-util"]},
        {"workspace": True, "features": ["diagnostics", "test-util"]},
    ):
        raise ValueError("unexpected test runtime features for Core")
    return ["default", *dependency["features"]]


def adapt_inputs(source, dependency, configuration):
    """Change only stack collection and the two Cargo files that locate the private crate."""
    manifest = source / "Cargo.toml"
    lock = source / "Cargo.lock"
    workspace = tomllib.loads(manifest.read_text(encoding="utf-8"))
    adapter = tomllib.loads((source / "crates/rca-tokio/Cargo.toml").read_text(encoding="utf-8"))
    dev = adapter["dev-dependencies"]
    if (
        workspace.get("patch")
        or workspace["workspace"]["dependencies"].get("dhat") != "0.3.3"
        or dev.get("dhat") != {"workspace": True}
        or adapter["features"].get("qualification-dhat") != []
        or ("rca-test" in dev) != (configuration == "deterministic")
    ):
        raise ValueError("unexpected allocation benchmark configuration")
    pinned = [p for p in tomllib.loads(lock.read_text(encoding="utf-8"))["package"] if p["name"] == "dhat"]
    if len(pinned) != 1 or (pinned[0]["version"], pinned[0].get("source"), pinned[0].get("checksum")) != (
        "0.3.3",
        REGISTRY,
        ARCHIVE,
    ):
        raise ValueError("unexpected allocator in the product lockfile")

    library = dependency / "src/lib.rs"
    if digest(library) != STOCK:
        raise ValueError("unexpected allocation profiler source")
    original = library.read_bytes()
    start = original.index(b"macro_rules! new_backtrace {")
    end = original.index(b"\n// Get a backtrace, possibly trimmed.", start)
    amended = original[:start] + EMPTY_STACK + original[end:]
    if hashlib.sha256(amended).hexdigest() != PATCHED:
        raise ValueError("unexpected aggregate allocator patch")
    library.write_bytes(amended)

    section = b"[workspace.dependencies]\n"
    block = b'[[package]]\nname = "dhat"\nversion = "0.3.3"\n'
    registry = f'source = "{REGISTRY}"\nchecksum = "{ARCHIVE}"\n'.encode()
    manifest_bytes, lock_bytes = manifest.read_bytes(), lock.read_bytes()
    if manifest_bytes.count(section) != 1 or lock_bytes.count(block + registry) != 1:
        raise ValueError("unexpected allocation manifest or lockfile layout")
    patch = b'[patch.crates-io]\ndhat = { path = "../dhat-0.3.3" }\n\n'
    manifest.write_bytes(manifest_bytes.replace(section, patch + section))
    lock.write_bytes(lock_bytes.replace(block + registry, block))


def stage(source, manifest, expected, archive, destination, configuration):
    """Build an independent source directory. Leave the registry and both original inputs untouched."""
    source = source.absolute()
    destination = destination.absolute()
    if (
        configuration not in CONFIGURATIONS
        or source != source.resolve(strict=True)
        or destination.exists()
        or destination.is_symlink()
        or destination.resolve() != destination
        or destination.is_relative_to(source)
    ):
        raise ValueError("expected a new, separate destination and a known native configuration")
    source_record = _regular(manifest.absolute()).read_bytes()
    if hashlib.sha256(source_record).hexdigest() != expected or files(source) != json.loads(source_record):
        raise ValueError("product source does not match the pinned source manifest")
    with _regular(archive.absolute()).open("rb") as stream:
        saved = stream.read(32_009)  # The pinned crate is 32,008 bytes.
    if hashlib.sha256(saved).hexdigest() != ARCHIVE:
        raise ValueError("unexpected allocation profiler archive")
    destination.mkdir()
    try:
        with tarfile.open(fileobj=io.BytesIO(saved), mode="r:gz") as crate:
            crate.extractall(destination, filter="data")
        product, dependency = destination / "source", destination / CRATE
        shutil.copytree(source, product, symlinks=True)
        if files(product) != json.loads(source_record):
            raise ValueError("product source changed during preparation")
        adapt_inputs(product, dependency, configuration)
        core_features(product, configuration)
        receipt = {
            "configuration": configuration,
            "source_manifest_sha256": expected,
            "archive_sha256": ARCHIVE,
            "source_tree_sha256": tree_digest(product),
            "dependency_tree_sha256": tree_digest(dependency),
        }
        path = destination / "counts-source.json"
        path.write_text(json.dumps(receipt, indent=2) + "\n", encoding="utf-8")
        return digest(path)
    except BaseException:
        shutil.rmtree(destination)
        raise


def check_build(directory, expected, log, binary):
    """Check Cargo's reported profile, source roots and new executable for this instrument."""
    directory = directory.resolve(strict=True)
    path = _regular(directory / "counts-source.json")
    product, dependency = directory / "source", directory / CRATE
    receipt = json.loads(path.read_bytes())
    if (
        digest(path) != expected
        or receipt["archive_sha256"] != ARCHIVE
        or receipt["configuration"] not in CONFIGURATIONS
        or receipt["source_tree_sha256"] != tree_digest(product)
        or receipt["dependency_tree_sha256"] != tree_digest(dependency)
        or digest(dependency / "src/lib.rs") != PATCHED
    ):
        raise ValueError("allocation build inputs differ from the pinned preparation")
    records = [row for row in _records(log) if row.get("reason") == "compiler-artifact"]
    for row in records:
        if row["package_id"].startswith(REGISTRY + "#"):
            continue
        root = _regular(Path(row["manifest_path"])).parent
        if (
            not row["package_id"].startswith("path+")
            or row.get("fresh") is not False
            or not (root.is_relative_to(product) or root == dependency)
            or not _regular(Path(row["target"]["src_path"])).is_relative_to(root)
        ):
            raise ValueError("allocation build reused an unverified unit or used a different source")
    expected_units = [
        ("dhat", dependency, "src/lib.rs", [], "lib"),
        (
            "rca_core",
            product / "crates/rca-core",
            "src/lib.rs",
            core_features(product, receipt["configuration"]),
            "lib",
        ),
        ("rca_tokio", product / "crates/rca-tokio", "src/lib.rs", ["qualification-dhat"], "lib"),
        ("qualification", product / "crates/rca-tokio", "benches/qualification.rs", ["qualification-dhat"], "bench"),
    ]
    if receipt["configuration"] == "deterministic":
        expected_units.append(("rca_test", product / "crates/rca-test", "src/lib.rs", [], "lib"))
    elif any(row["target"]["name"] == "rca_test" for row in records):
        raise ValueError("native allocator build contains the deterministic test runtime")
    for name, root, relative, features, kind in expected_units:
        selected = [r for r in records if r["target"]["name"] == name]
        if len(selected) != 1:
            raise ValueError(f"expected one compiler record for {name}")
        row = selected[0]
        profile = {
            "opt_level": "3",
            "debuginfo": 2,
            "debug_assertions": False,
            "overflow_checks": False,
            "test": kind == "bench",
        }
        if (
            not row["package_id"].startswith("path+" + root.as_uri() + "#")
            or row.get("fresh") is not False
            or row["features"] != features
            or row["profile"] != profile
            or row["target"]["kind"] != [kind]
            or Path(row["manifest_path"]) != root / "Cargo.toml"
            or Path(row["target"]["src_path"]) != root / relative
        ):
            raise ValueError(f"allocation benchmark configuration differs: {name}")
        if kind == "bench" and (
            row["fresh"] is not False
            or Path(row["executable"]) != _regular(binary)
            or row["executable"] not in row["filenames"]
        ):
            raise ValueError("allocation benchmark binary differs from the fresh build")
    return digest(binary)


def check_sample(profile, row):
    """Reject detailed, incomplete or inconsistent heap evidence."""
    points = profile.get("pps")
    if (
        profile.get("dhatFileVersion") != 2
        or profile.get("mode") != "rust-heap"
        or profile.get("ftbl") != ["[root]"]
        or not isinstance(points, list)
        or len(points) != 1
        or not isinstance(points[0], dict)
        or points[0].get("fs") != []
        or type(points[0].get("gb")) is not int
        or points[0]["gb"] != points[0].get("mb")
    ):
        raise ValueError("expected one aggregate allocation point with no stack")
    for metric, field in (("allocations", "tbk"), ("allocated_bytes", "tb"), ("peak_bytes", "mb")):
        value = row.get(metric)
        if (
            type(value) is not int
            or value <= 0
            or type(points[0].get(field)) is not int
            or value != points[0][field]
        ):
            raise ValueError(f"aggregate heap profile disagrees with the measured {metric}")


def main(argv):
    """Handle explicit preparation and checks. Neither operation qualifies a release."""
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    prepare = commands.add_parser("stage", help="stage the counter in a new directory")
    for arg in ("source", "manifest", "archive", "destination"):
        prepare.add_argument(arg, type=Path)
    prepare.add_argument("--manifest-sha256", required=True)
    prepare.add_argument("--configuration", choices=CONFIGURATIONS, required=True)
    build = commands.add_parser("check-build", help="check the source and compiler transcript")
    for arg in ("directory", "log", "binary"):
        build.add_argument(arg, type=Path)
    build.add_argument("--receipt-sha256", required=True)
    sample = commands.add_parser("check-sample", help="compare an aggregate profile with the workload output")
    sample.add_argument("profile", type=Path)
    sample.add_argument("measurement", type=Path)
    args = parser.parse_args(argv)
    if args.operation == "stage":
        print(
            stage(
                args.source,
                args.manifest,
                args.manifest_sha256,
                args.archive,
                args.destination,
                args.configuration,
            )
        )
    elif args.operation == "check-build":
        print(check_build(args.directory, args.receipt_sha256, args.log.absolute(), args.binary.absolute()))
    else:
        check_sample(json.loads(args.profile.read_bytes()), json.loads(args.measurement.read_bytes()))
