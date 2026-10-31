#!/usr/bin/env python3

import argparse
import hashlib
import json
import pathlib
import shutil
import sys
import tarfile
import tempfile
import tomllib


TRANSPORT_PACKAGES = frozenset(
    {"rca-event-listener", "rca-event-listener-strategy", "rca-async-channel"}
)
RCA_PACKAGES = TRANSPORT_PACKAGES | frozenset(
    {
        "rca-core",
        "rca-macros",
        "rca-persistence",
        "rca-test",
        "rca-tokio",
        "rust-composable-architecture",
    }
)
PUBLIC_UMBRELLA_FEATURES = frozenset(
    {"diagnostics", "futures-stream", "local", "macros", "test-util", "tokio"}
)
EXPECTED_FEATURES = {
    "rca-event-listener": {
        "default": ["std"],
        "loom": ["parking?/loom", "dep:loom"],
        "portable-atomic": ["portable-atomic-util", "portable_atomic_crate"],
        "std": ["parking"],
    },
    "rca-event-listener-strategy": {
        "default": ["std"],
        "loom": ["event-listener/loom"],
        "portable-atomic": ["event-listener/portable-atomic"],
        "std": ["event-listener/std"],
    },
    "rca-async-channel": {
        "default": ["std"],
        "portable-atomic": [
            "concurrent-queue/portable-atomic",
            "event-listener-strategy/portable-atomic",
            "dep:portable-atomic-util",
            "dep:portable-atomic",
        ],
        "std": ["concurrent-queue/std", "event-listener-strategy/std"],
    },
    "rca-core": {
        "allocation-measurement": [],
        "default": [],
        "diagnostics": [],
        "futures-stream": ["dep:futures-core"],
        "local": [],
        "test-util": [],
    },
    "rca-macros": {},
    "rca-persistence": {"default": [], "file": ["dep:tokio", "dep:tempfile"]},
    "rca-test": {"local": ["rca-core/local"]},
    "rca-tokio": {"qualification-dhat": []},
    "rust-composable-architecture": {
        "default": [],
        "diagnostics": ["rca-core/diagnostics"],
        "futures-stream": ["rca-core/futures-stream"],
        "local": ["rca-core/local"],
        "macros": ["dep:rca-macros"],
        "test-util": ["dep:rca-test", "rca-core/test-util"],
        "tokio": ["dep:rca-tokio"],
    },
}
REQUIRED_PACKAGE_FILES = frozenset(
    {"LICENSE-APACHE.md", "LICENSE-MIT.md", "README.md"}
)
REQUIRED_TRANSPORT_FILES = frozenset(
    {"LICENSE-APACHE", "LICENSE-MIT", "README.md", "UPSTREAM.md", "Cargo.toml.upstream"}
)
CRATES_IO_INDEX = "https://github.com/rust-lang/crates.io-index"
FIXTURE_ROOTS = frozenset({"rca-core", "rust-composable-architecture"})
FIXTURE_FEATURES = {
    "rca-core": frozenset({"diagnostics", "futures-stream", "local"}),
    "rust-composable-architecture": PUBLIC_UMBRELLA_FEATURES,
    "rca-persistence": frozenset({"file"}),
    "rca-test": frozenset({"local"}),
}


def dependency_tables(manifest: dict[str, object]) -> list[dict[str, object]]:
    tables = [
        manifest.get("dependencies", {}),
        manifest.get("dev-dependencies", {}),
        manifest.get("build-dependencies", {}),
    ]
    for target in manifest.get("target", {}).values():
        tables.extend(
            [
                target.get("dependencies", {}),
                target.get("dev-dependencies", {}),
                target.get("build-dependencies", {}),
            ]
        )
    return tables


def validate_fixtures(
    expected_version: str, fixture_manifests: list[pathlib.Path]
) -> None:
    for manifest_path in fixture_manifests:
        if not manifest_path.is_file():
            raise FileNotFoundError(f"downstream fixture manifest missing: {manifest_path}")
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        seen_packages: dict[str, str] = {}
        for table in dependency_tables(manifest):
            for alias, raw_specification in table.items():
                specification = (
                    {"version": raw_specification}
                    if isinstance(raw_specification, str)
                    else raw_specification
                )
                if not isinstance(specification, dict):
                    raise TypeError(f"invalid dependency specification for {alias}")
                if "path" in specification:
                    raise ValueError(f"path dependency {alias} found in {manifest_path}")
                if "git" in specification:
                    raise ValueError(f"git dependency {alias} found in {manifest_path}")
                package = specification.get("package")
                actual_name = package if isinstance(package, str) else alias
                if actual_name not in RCA_PACKAGES:
                    if actual_name.startswith("rca-"):
                        raise ValueError(f"unknown RCA dependency {actual_name} in {manifest_path}")
                    continue
                if actual_name not in FIXTURE_FEATURES:
                    raise ValueError(
                        f"direct RCA implementation dependency {actual_name} found in "
                        f"{manifest_path}"
                    )
                previous_alias = seen_packages.get(actual_name)
                if previous_alias is not None and previous_alias != alias:
                    raise ValueError(f"duplicate RCA dependency {actual_name} in {manifest_path}")
                seen_packages[actual_name] = alias
                if alias == actual_name or package != actual_name:
                    raise ValueError(f"RCA dependency is not renamed in {manifest_path}")
                if specification.get("registry") != "rca-local":
                    raise ValueError(
                        f"RCA dependency does not use rca-local in {manifest_path}"
                    )
                if specification.get("version") != f"={expected_version}":
                    raise ValueError(f"RCA dependency version is not exact in {manifest_path}")
                features = specification.get("features", [])
                if not isinstance(features, list) or not all(
                    isinstance(feature, str) for feature in features
                ):
                    raise TypeError(f"invalid RCA feature list in {manifest_path}")
                private_features = set(features) - FIXTURE_FEATURES[actual_name]
                if private_features:
                    raise ValueError(
                        f"private or unknown RCA features in {manifest_path}: "
                        + ", ".join(sorted(private_features))
                    )
        root_dependencies = len(FIXTURE_ROOTS.intersection(seen_packages))
        if root_dependencies != 1:
            raise ValueError(
                f"expected one renamed RCA root dependency in {manifest_path}, "
                f"found {root_dependencies}"
            )


def index_path(index_root: pathlib.Path, name: str) -> pathlib.Path:
    lowered = name.lower()
    if len(lowered) == 1:
        relative = pathlib.Path("1", lowered)
    elif len(lowered) == 2:
        relative = pathlib.Path("2", lowered)
    elif len(lowered) == 3:
        relative = pathlib.Path("3", lowered[0], lowered)
    else:
        relative = pathlib.Path(lowered[:2], lowered[2:4], lowered)
    return index_root / relative


def dependency_entries(
    table: dict[str, object] | None,
    kind: str,
    target: str | None,
) -> list[dict[str, object]]:
    entries: list[dict[str, object]] = []
    for alias, raw_specification in sorted((table or {}).items()):
        specification = (
            {"version": raw_specification}
            if isinstance(raw_specification, str)
            else raw_specification
        )
        if not isinstance(specification, dict):
            raise TypeError(f"invalid dependency specification for {alias}")
        package = specification.get("package")
        actual_name = package if isinstance(package, str) else alias
        entries.append(
            {
                "name": alias,
                "req": specification.get("version", "*"),
                "features": specification.get("features", []),
                "optional": specification.get("optional", False),
                "default_features": specification.get("default-features", True),
                "target": target,
                "kind": kind,
                "registry": None
                if actual_name in RCA_PACKAGES
                else CRATES_IO_INDEX,
                "package": package,
            }
        )
    return entries


def split_features(
    features: dict[str, list[str]],
) -> tuple[dict[str, list[str]], dict[str, list[str]]]:
    legacy: dict[str, list[str]] = {}
    namespaced: dict[str, list[str]] = {}
    for name, values in features.items():
        if any(value.startswith("dep:") or "?/" in value for value in values):
            namespaced[name] = values
        else:
            legacy[name] = values
    return legacy, namespaced


def reject_dependency_paths(
    manifest: dict[str, object], archive: pathlib.Path
) -> None:
    for table in dependency_tables(manifest):
        for dependency, specification in table.items():
            if isinstance(specification, dict) and "path" in specification:
                raise ValueError(f"path dependency {dependency} found in {archive}")


def archive_index_entry(
    archive: pathlib.Path,
    expected_version: str,
    expected_rust_version: str,
) -> tuple[str, dict[str, object]]:
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory() as extraction_directory:
        extraction_root = pathlib.Path(extraction_directory)
        with tarfile.open(archive, "r:gz") as package_archive:
            for member in package_archive.getmembers():
                member_path = pathlib.PurePosixPath(member.name)
                if member_path.is_absolute() or ".." in member_path.parts:
                    raise ValueError(f"unsafe archive member: {member.name}")
                if not (member.isfile() or member.isdir()):
                    raise ValueError(f"unsupported archive member: {member.name}")
            package_archive.extractall(extraction_root, filter="data")
        manifests = list(extraction_root.glob("*/Cargo.toml"))
        if len(manifests) != 1:
            raise ValueError(f"expected one normalized manifest in {archive}")
        manifest_path = manifests[0]
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        reject_dependency_paths(manifest, archive)
        archive_root_name = manifest_path.parent.name
        archive_files = {
            path.relative_to(manifest_path.parent).as_posix()
            for path in manifest_path.parent.rglob("*")
            if path.is_file()
        }

    package = manifest["package"]
    name = package["name"]
    version = package["version"]
    if name not in RCA_PACKAGES:
        raise ValueError(f"unexpected RCA archive package {name}")
    if version != expected_version:
        raise ValueError(
            f"archive {archive} has version {version}, expected {expected_version}"
        )
    if archive.name != f"{name}-{version}.crate":
        raise ValueError(f"archive filename does not match manifest: {archive}")
    if archive_root_name != f"{name}-{version}":
        raise ValueError(f"archive root does not match manifest: {archive}")
    if name == "rca-core" and "dhat-heap.json" in archive_files:
        raise ValueError(f"archive {archive} contains local allocator output: dhat-heap.json")
    edition = "2021" if name in TRANSPORT_PACKAGES else "2024"
    if package.get("edition") != edition:
        raise ValueError(f"archive {archive} does not use edition {edition}")
    if package.get("rust-version") != expected_rust_version:
        raise ValueError(
            f"archive {archive} has rust-version {package.get('rust-version')}, "
            f"expected {expected_rust_version}"
        )
    license_expression = "Apache-2.0 OR MIT" if name in TRANSPORT_PACKAGES else "MIT OR Apache-2.0"
    if package.get("license") != license_expression:
        raise ValueError(f"archive {archive} has unexpected license metadata")
    readme = package.get("readme")
    if not isinstance(readme, str):
        raise ValueError(f"archive {archive} has no packaged readme metadata")
    readme_path = pathlib.PurePosixPath(readme)
    if readme_path.is_absolute() or ".." in readme_path.parts:
        raise ValueError(f"archive {archive} has unsafe packaged readme path")
    if readme_path.as_posix() not in archive_files:
        raise ValueError(f"archive {archive} has no packaged readme")
    required_files = REQUIRED_TRANSPORT_FILES if name in TRANSPORT_PACKAGES else REQUIRED_PACKAGE_FILES
    missing_files = sorted(required_files - archive_files)
    if missing_files:
        raise ValueError(
            f"archive {archive} is missing required package files: "
            + ", ".join(missing_files)
        )
    features = manifest.get("features", {})
    if features != EXPECTED_FEATURES[name]:
        raise ValueError(
            f"archive {archive} has feature contract {features!r}, "
            f"expected {EXPECTED_FEATURES[name]!r}"
        )

    for table in dependency_tables(manifest):
        for alias, raw_specification in table.items():
            specification = (
                {"version": raw_specification}
                if isinstance(raw_specification, str)
                else raw_specification
            )
            if not isinstance(specification, dict):
                raise TypeError(f"invalid dependency specification for {alias}")
            package_name = specification.get("package")
            actual_name = package_name if isinstance(package_name, str) else alias
            if actual_name.startswith("rca-") and actual_name not in RCA_PACKAGES:
                raise ValueError(f"archive {archive} has unknown RCA dependency {actual_name}")
            if (
                actual_name in RCA_PACKAGES
                and specification.get("version") != f"={expected_version}"
            ):
                raise ValueError(
                    f"archive {archive} has non-exact RCA dependency {actual_name}"
                )

    dependencies: list[dict[str, object]] = []
    dependencies.extend(dependency_entries(manifest.get("dependencies"), "normal", None))
    dependencies.extend(dependency_entries(manifest.get("dev-dependencies"), "dev", None))
    dependencies.extend(dependency_entries(manifest.get("build-dependencies"), "build", None))
    for target, target_tables in sorted(manifest.get("target", {}).items()):
        dependencies.extend(
            dependency_entries(target_tables.get("dependencies"), "normal", target)
        )
        dependencies.extend(
            dependency_entries(target_tables.get("dev-dependencies"), "dev", target)
        )
        dependencies.extend(
            dependency_entries(target_tables.get("build-dependencies"), "build", target)
        )

    legacy_features, features2 = split_features(manifest.get("features", {}))
    entry: dict[str, object] = {
        "name": name,
        "vers": version,
        "deps": dependencies,
        "cksum": checksum,
        "features": legacy_features,
        "yanked": False,
        "links": package.get("links"),
        "rust_version": package.get("rust-version"),
        "v": 2,
    }
    if features2:
        entry["features2"] = features2
    return name, entry


def stage_archives(
    index_root: pathlib.Path,
    downloads_root: pathlib.Path,
    archives: list[pathlib.Path],
    expected_version: str,
    expected_rust_version: str,
) -> None:
    staged = [
        (
            archive,
            *archive_index_entry(archive, expected_version, expected_rust_version),
        )
        for archive in archives
    ]
    names = [name for _, name, _ in staged]
    if len(names) != len(RCA_PACKAGES) or set(names) != RCA_PACKAGES:
        missing = sorted(RCA_PACKAGES - set(names))
        duplicates = sorted(name for name in set(names) if names.count(name) > 1)
        raise ValueError(
            "archive set mismatch: "
            f"missing={missing or 'none'}, duplicates={duplicates or 'none'}"
        )

    for archive, name, entry in staged:
        write_index_entry(index_root, name, entry)
        shutil.copy2(archive, downloads_root / archive.name)
    write_registry_config(index_root, downloads_root)


def write_index_entry(
    index_root: pathlib.Path, name: str, entry: dict[str, object]
) -> None:
    destination = index_path(index_root, name)
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text(
        json.dumps(entry, separators=(",", ":"), sort_keys=True) + "\n",
        encoding="utf-8",
    )


def write_registry_config(
    index_root: pathlib.Path, downloads_root: pathlib.Path
) -> None:
    config = {
        "dl": downloads_root.as_uri() + "/{crate}-{version}.crate",
        "api": None,
    }
    (index_root / "config.json").write_text(
        json.dumps(config, separators=(",", ":"), sort_keys=True) + "\n",
        encoding="utf-8",
    )


def write_cargo_config(index_root: pathlib.Path, config_path: pathlib.Path) -> None:
    config_path.write_text(
        f'[registries.rca-local]\nindex = "{index_root.as_uri()}"\n',
        encoding="utf-8",
    )


def validate_registry_source(
    metadata_path: pathlib.Path, package_name: str, registry_index: str
) -> None:
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    packages = [
        package
        for package in metadata.get("packages", [])
        if package.get("name") == package_name
    ]
    expected_source = f"registry+{registry_index}"
    if len(packages) != 1 or packages[0].get("source") != expected_source:
        actual_sources = [package.get("source") for package in packages]
        raise ValueError(
            f"{package_name} was not resolved from the temporary registry "
            f"{expected_source}: {actual_sources}"
        )


def validate_consumer_source(
    metadata_path: pathlib.Path, expected_version: str, registry_index: str
) -> None:
    packages = json.loads(metadata_path.read_text(encoding="utf-8"))["packages"]
    names = {package["name"] for package in packages}
    if names & {"rca-minimal-core-downstream", "rca-minimal-facade-downstream"}:
        forbidden = names & {"rca-persistence", "rca-test", "rca-macros", "tokio", "tempfile", "insta"}
        if forbidden:
            raise ValueError(f"minimal consumer acquired optional dependencies: {sorted(forbidden)}")
    if "rca-persistence-downstream" in names:
        forbidden = names & {"tokio", "tempfile", "insta"}
        if forbidden:
            raise ValueError(f"generic persistence consumer acquired optional dependencies: {sorted(forbidden)}")
    for package in packages:
        if package["name"] not in RCA_PACKAGES:
            continue
        if package["version"] != expected_version:
            raise ValueError(f"unexpected RCA version for {package['name']}: {package['version']}")
        validate_registry_source(metadata_path, package["name"], registry_index)
    print("validated registry sources: " + ", ".join(sorted(names & RCA_PACKAGES)))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    validate = subparsers.add_parser("validate-fixtures")
    validate.add_argument("--version", required=True)
    validate.add_argument("manifests", nargs="+", type=pathlib.Path)

    stage = subparsers.add_parser("stage-archives")
    stage.add_argument("--index", required=True, type=pathlib.Path)
    stage.add_argument("--downloads", required=True, type=pathlib.Path)
    stage.add_argument("--version", required=True)
    stage.add_argument("--rust-version", required=True)
    stage.add_argument("archives", nargs="+", type=pathlib.Path)

    config = subparsers.add_parser("write-config")
    config.add_argument("--index", required=True, type=pathlib.Path)
    config.add_argument("config", type=pathlib.Path)

    source = subparsers.add_parser("validate-registry-source")
    source.add_argument("--metadata", required=True, type=pathlib.Path)
    source.add_argument("--package", required=True)
    source.add_argument("--registry-index", required=True)
    consumer = subparsers.add_parser("validate-consumer-source")
    consumer.add_argument("--metadata", required=True, type=pathlib.Path)
    consumer.add_argument("--version", required=True)
    consumer.add_argument("--registry-index", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.command == "validate-fixtures":
        validate_fixtures(args.version, args.manifests)
    elif args.command == "stage-archives":
        stage_archives(
            args.index,
            args.downloads,
            args.archives,
            args.version,
            args.rust_version,
        )
    elif args.command == "write-config":
        write_cargo_config(args.index, args.config)
    elif args.command == "validate-registry-source":
        validate_registry_source(args.metadata, args.package, args.registry_index)
    elif args.command == "validate-consumer-source":
        validate_consumer_source(args.metadata, args.version, args.registry_index)
    return 0


if __name__ == "__main__":
    sys.exit(main())
