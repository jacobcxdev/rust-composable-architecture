import io
import hashlib
import json
import pathlib
import os
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest

from scripts import package_registry


VALID_FIXTURE = """\
[package]
name = "consumer"
version = "0.0.0"

[dependencies]
architecture = { package = "rust-composable-architecture", version = "=0.1.0", registry = "rca-local", features = ["macros"] }
"""

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
        "portable-atomic": ["concurrent-queue/portable-atomic", "event-listener-strategy/portable-atomic", "dep:portable-atomic-util", "dep:portable-atomic"],
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


class FixtureValidationTests(unittest.TestCase):
    def write_manifest(self, contents: str) -> pathlib.Path:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        manifest = pathlib.Path(directory.name) / "Cargo.toml"
        manifest.write_text(contents, encoding="utf-8")
        return manifest

    def test_accepts_one_renamed_registry_only_umbrella(self) -> None:
        package_registry.validate_fixtures(
            "0.1.0", [self.write_manifest(VALID_FIXTURE)]
        )

    def test_accepts_same_root_alias_in_multiple_dependency_tables(self) -> None:
        for table in (
            "dev-dependencies",
            "build-dependencies",
            "target.'cfg(unix)'.dependencies",
            "target.'cfg(unix)'.dev-dependencies",
            "target.'cfg(unix)'.build-dependencies",
        ):
            for package, first_feature, second_feature in (
                ("rust-composable-architecture", "tokio", "test-util"),
                ("rca-core", "diagnostics", "local"),
            ):
                with self.subTest(table=table, package=package):
                    manifest = VALID_FIXTURE.replace(
                        "rust-composable-architecture", package
                    ).replace('"macros"', f'"{first_feature}"')
                    manifest += (
                        f"\n[{table}]\n"
                        f'architecture = {{ package = "{package}", '
                        'version = "=0.1.0", registry = "rca-local", '
                        f'features = ["{second_feature}"] }}\n'
                    )
                    package_registry.validate_fixtures(
                        "0.1.0", [self.write_manifest(manifest)]
                    )

    def test_rejects_different_alias_for_repeated_root(self) -> None:
        manifest = self.write_manifest(
            VALID_FIXTURE
            + '\n[dev-dependencies]\n'
            + 'other = { package = "rust-composable-architecture", '
            + 'version = "=0.1.0", registry = "rca-local" }\n'
        )

        with self.assertRaisesRegex(ValueError, "duplicate RCA dependency"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_revalidates_each_repeated_root_declaration(self) -> None:
        for fields, error, message in (
            ('path = "../local"', ValueError, "path dependency"),
            ('git = "https://invalid.example/repo"', ValueError, "git dependency"),
            ('registry = "crates-io"', ValueError, "does not use rca-local"),
            ('version = "0.1"', ValueError, "version is not exact"),
            ('features = "test-util"', TypeError, "invalid RCA feature list"),
            ('features = [1]', TypeError, "invalid RCA feature list"),
            ('features = ["private"]', ValueError, "private or unknown RCA features"),
        ):
            with self.subTest(fields=fields):
                # Override one field in the second occurrence, not the first.
                specification = (
                    'package = "rust-composable-architecture", '
                    'version = "=0.1.0", registry = "rca-local"'
                )
                if fields.startswith("registry ="):
                    specification = specification.replace('registry = "rca-local"', fields)
                elif fields.startswith("version ="):
                    specification = specification.replace('version = "=0.1.0"', fields)
                else:
                    specification += f", {fields}"
                manifest = self.write_manifest(
                    VALID_FIXTURE
                    + '\n[dev-dependencies]\n'
                    + f'architecture = {{ {specification} }}\n'
                )

                with self.assertRaisesRegex(error, message):
                    package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_distinct_root_in_another_dependency_table(self) -> None:
        manifest = self.write_manifest(
            VALID_FIXTURE
            + '\n[dev-dependencies]\n'
            + 'core = { package = "rca-core", version = "=0.1.0", '
            + 'registry = "rca-local" }\n'
        )

        with self.assertRaisesRegex(ValueError, "expected one renamed RCA root"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_accepts_diagnostics_umbrella_feature(self) -> None:
        package_registry.validate_fixtures(
            "0.1.0", [self.write_manifest(VALID_FIXTURE.replace('"macros"', '"diagnostics"'))]
        )

    def test_accepts_local_feature_on_facade_core_and_test_support(self) -> None:
        for package in ("rust-composable-architecture", "rca-core"):
            with self.subTest(package=package):
                manifest = VALID_FIXTURE.replace("rust-composable-architecture", package).replace('"macros"', '"local"')
                manifest += 'testing = { package = "rca-test", version = "=0.1.0", registry = "rca-local", features = ["local"] }\n'
                package_registry.validate_fixtures("0.1.0", [self.write_manifest(manifest)])

    def test_accepts_exact_renamed_persistence_and_controls(self) -> None:
        package_registry.validate_fixtures("0.1.0", [self.write_manifest(
            VALID_FIXTURE
            + 'documents = { package = "rca-persistence", version = "=0.1.0", registry = "rca-local", default-features = false, features = ["file"] }\n'
            + 'testing = { package = "rca-test", version = "=0.1.0", registry = "rca-local" }\n'
        )])

    def test_accepts_runtime_neutral_core_consumer(self) -> None:
        package_registry.validate_fixtures("0.1.0", [self.write_manifest(
            VALID_FIXTURE.replace('rust-composable-architecture', 'rca-core')
            .replace('["macros"]', '[]')
        )])

    def test_rejects_inexact_persistence_dependency(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE +
            'documents = { package = "rca-persistence", version = "0.1", registry = "rca-local", default-features = false }\n')
        with self.assertRaisesRegex(ValueError, "version is not exact"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_git_persistence_dependency(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE +
            'documents = { package = "rca-persistence", version = "=0.1.0", registry = "rca-local", git = "https://invalid.example/repo" }\n')
        with self.assertRaisesRegex(ValueError, "git dependency"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_unknown_persistence_feature(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE +
            'documents = { package = "rca-persistence", version = "=0.1.0", registry = "rca-local", default-features = false, features = ["snapshot"] }\n')
        with self.assertRaisesRegex(ValueError, "private or unknown RCA features"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_additional_core_root(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE +
            'core = { package = "rca-core", version = "=0.1.0", registry = "rca-local" }\n')
        with self.assertRaisesRegex(ValueError, "expected one renamed RCA root"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_runtime_add_on(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE +
            'host = { package = "rca-tokio", version = "=0.1.0", registry = "rca-local" }\n')
        with self.assertRaisesRegex(ValueError, "direct RCA implementation dependency"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_ecosystem_consumer_manifests_are_valid(self) -> None:
        root = pathlib.Path(__file__).resolve().parents[2] / "tests"
        package_registry.validate_fixtures("0.1.0", [root / name / "Cargo.toml" for name in (
            "downstream-minimal-core", "downstream-minimal-facade",
            "downstream-ecosystem", "downstream-persistence", "downstream-file",
        )])

    def test_rejects_unknown_rca_add_on(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE + 'unknown = { package = "rca-unknown", version = "=0.1.0", registry = "rca-local" }\n')
        with self.assertRaisesRegex(ValueError, "unknown RCA dependency"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_duplicate_persistence_add_on(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE + 'one = { package = "rca-persistence", version = "=0.1.0", registry = "rca-local" }\ntwo = { package = "rca-persistence", version = "=0.1.0", registry = "rca-local" }\n')
        with self.assertRaisesRegex(ValueError, "duplicate RCA dependency"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_path_dependencies(self) -> None:
        manifest = self.write_manifest(
            VALID_FIXTURE
            + '\nhelper = { package = "helper", path = "../helper" }\n'
        )

        with self.assertRaisesRegex(ValueError, "path dependency helper"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_unknown_umbrella_features(self) -> None:
        manifest = self.write_manifest(VALID_FIXTURE.replace('"macros"', '"private"'))

        with self.assertRaisesRegex(ValueError, "private or unknown RCA features"):
            package_registry.validate_fixtures("0.1.0", [manifest])

    def test_rejects_direct_implementation_dependency(self) -> None:
        manifest = self.write_manifest(
            VALID_FIXTURE + '\ncore = { package = "rca-core", version = "=0.1.0" }\n'
        )

        with self.assertRaisesRegex(ValueError, "does not use rca-local"):
            package_registry.validate_fixtures("0.1.0", [manifest])


class ArchiveStagingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        self.index = self.root / "index"
        self.downloads = self.root / "downloads"
        self.index.mkdir()
        self.downloads.mkdir()

    def make_archive(
        self,
        name: str,
        *,
        version: str = "0.1.0",
        rust_version: str = "1.98.0",
        dependencies: str = "",
        include_readme: bool = True,
        include_apache_license: bool = True,
        include_mit_license: bool = True,
        include_allocator_output: bool = False,
        include_provenance: bool = True,
        features: dict[str, list[str]] | None = None,
    ) -> pathlib.Path:
        archive = self.root / f"{name}-{version}.crate"
        package_root = f"{name}-{version}"
        transport = name in {"rca-event-listener", "rca-event-listener-strategy", "rca-async-channel"}
        manifest = f'''\
[package]
name = "{name}"
version = "{version}"
edition = "{'2021' if transport else '2024'}"
rust-version = "{rust_version}"
license = "{'Apache-2.0 OR MIT' if transport else 'MIT OR Apache-2.0'}"
readme = "README.md"

[features]
{self.feature_toml(EXPECTED_FEATURES[name] if features is None else features)}

[dependencies]
{dependencies}
'''
        with tarfile.open(archive, "w:gz") as package_archive:
            self.add_member(package_archive, f"{package_root}/Cargo.toml", manifest)
            if include_allocator_output:
                self.add_member(package_archive, f"{package_root}/dhat-heap.json", "{}\n")
            if include_readme:
                self.add_member(package_archive, f"{package_root}/README.md", "# RCA\n")
            if include_apache_license:
                self.add_member(
                    package_archive, f"{package_root}/LICENSE-APACHE{'' if transport else '.md'}", "Apache-2.0\n"
                )
            if include_mit_license:
                self.add_member(
                    package_archive, f"{package_root}/LICENSE-MIT{'' if transport else '.md'}", "MIT\n"
                )
            if transport and include_provenance:
                self.add_member(package_archive, f"{package_root}/UPSTREAM.md", "Upstream source and changes.\n")
                self.add_member(package_archive, f"{package_root}/Cargo.toml.upstream", '[package]\nname = "upstream"\n')
        return archive

    @staticmethod
    def feature_toml(features: dict[str, list[str]]) -> str:
        return "\n".join(
            f"{name} = {json.dumps(values)}" for name, values in features.items()
        )

    @staticmethod
    def add_member(package_archive: tarfile.TarFile, name: str, contents: str) -> None:
        data = contents.encode("utf-8")
        member = tarfile.TarInfo(name)
        member.size = len(data)
        package_archive.addfile(member, io.BytesIO(data))

    def all_archives(self) -> list[pathlib.Path]:
        return [
            self.make_archive(name)
            for name in sorted(EXPECTED_FEATURES)
        ]

    def test_stages_exact_complete_archive_set(self) -> None:
        archives = self.all_archives()

        package_registry.stage_archives(
            self.index,
            self.downloads,
            archives,
            "0.1.0",
            "1.98.0",
        )

        self.assertEqual(
            {path.name for path in self.downloads.glob("*.crate")},
            {path.name for path in archives},
        )
        for name in package_registry.RCA_PACKAGES:
            entry = json.loads(
                package_registry.index_path(self.index, name).read_text(encoding="utf-8")
            )
            self.assertEqual(entry["name"], name)
            self.assertEqual(entry["vers"], "0.1.0")
            self.assertEqual(entry["rust_version"], "1.98.0")
            archive = self.downloads / f"{name}-0.1.0.crate"
            self.assertEqual(entry["cksum"], hashlib.sha256(archive.read_bytes()).hexdigest())

    def test_rejects_core_allocator_output_without_staging(self) -> None:
        archives = self.all_archives()
        self.make_archive("rca-core", include_allocator_output=True)

        with self.assertRaisesRegex(ValueError, "allocator output.*dhat-heap.json"):
            package_registry.stage_archives(
                self.index, self.downloads, archives, "0.1.0", "1.98.0"
            )

        self.assertEqual(list(self.downloads.iterdir()), [])
        self.assertEqual(list(self.index.iterdir()), [])

    def test_persistence_dependencies_stay_in_local_registry(self) -> None:
        archive = self.make_archive("rca-persistence", dependencies='rca-core = { version = "=0.1.0" }')
        name, entry = package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")
        self.assertEqual(name, "rca-persistence")
        self.assertEqual(entry["deps"][0]["req"], "=0.1.0")
        self.assertIsNone(entry["deps"][0]["registry"])

    def test_rejects_inexact_persistence_core_requirement(self) -> None:
        archive = self.make_archive("rca-persistence", dependencies='rca-core = { version = "0.1" }')
        with self.assertRaisesRegex(ValueError, "exact RCA dependency"):
            package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")

    def test_rejects_persistence_with_default_file_feature(self) -> None:
        archive = self.make_archive("rca-persistence", features={"default": ["file"], "file": ["dep:tokio", "dep:tempfile"]})
        with self.assertRaisesRegex(ValueError, "feature contract"):
            package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")

    def test_rejects_incomplete_archive_set_without_staging(self) -> None:
        archives = self.all_archives()[:-1]

        with self.assertRaisesRegex(ValueError, "archive set mismatch"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )

        self.assertEqual(list(self.downloads.iterdir()), [])
        self.assertEqual(list(self.index.iterdir()), [])

    def test_rejects_omitted_transport_archive_without_staging(self) -> None:
        archives = [archive for archive in self.all_archives()
                    if archive.name != "rca-event-listener-strategy-0.1.0.crate"]
        with self.assertRaisesRegex(ValueError, "archive set mismatch.*rca-event-listener-strategy"):
            package_registry.stage_archives(self.index, self.downloads, archives, "0.1.0", "1.98.0")
        self.assertEqual(list(self.downloads.iterdir()), [])
        self.assertEqual(list(self.index.iterdir()), [])

    def test_transport_dependencies_use_local_registry_with_renamed_packages(self) -> None:
        for owner, alias, dependency in (
            ("rca-core", "async-channel", "rca-async-channel"),
            ("rca-async-channel", "event-listener-strategy", "rca-event-listener-strategy"),
            ("rca-event-listener-strategy", "event-listener", "rca-event-listener"),
        ):
            with self.subTest(owner=owner):
                archive = self.make_archive(owner, dependencies=f'{alias} = {{ package = "{dependency}", version = "=0.1.0", default-features = false }}')
                _, entry = package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")
                self.assertEqual(entry["deps"], [{
                    "name": alias, "package": dependency, "req": "=0.1.0",
                    "registry": None, "features": [], "optional": False,
                    "default_features": False, "target": None, "kind": "normal",
                }])

    def test_rejects_invalid_transport_dependencies(self) -> None:
        for dependency, error in (
            ('package = "rca-async-channel", version = "0.1"', "non-exact RCA dependency"),
            ('package = "rca-async-channel", version = "=0.1.0", path = "../transport"', "path dependency"),
            ('package = "rca-async-chanel", version = "=0.1.0"', "unknown RCA dependency"),
        ):
            with self.subTest(dependency=dependency):
                archive = self.make_archive("rca-core", dependencies=f'async-channel = {{ {dependency} }}')
                with self.assertRaisesRegex(ValueError, error):
                    package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")

    def test_rejects_transport_without_upstream_provenance(self) -> None:
        archive = self.make_archive("rca-event-listener", include_provenance=False)
        with self.assertRaisesRegex(ValueError, "missing required package files.*Cargo.toml.upstream.*UPSTREAM.md"):
            package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")

    def test_rejects_transport_archive_wrong_version_or_filename(self) -> None:
        archive = self.make_archive("rca-async-channel", version="0.2.0")
        with self.assertRaisesRegex(ValueError, "version 0.2.0, expected 0.1.0"):
            package_registry.archive_index_entry(archive, "0.1.0", "1.98.0")
        archive = self.make_archive("rca-async-channel")
        renamed = archive.rename(self.root / "rca-event-listener-0.1.0.crate")
        with self.assertRaisesRegex(ValueError, "filename does not match manifest"):
            package_registry.archive_index_entry(renamed, "0.1.0", "1.98.0")

    def test_rejects_wrong_rust_version(self) -> None:
        archives = self.all_archives()
        archives[0] = self.make_archive(
            archives[0].name.removesuffix("-0.1.0.crate"),
            rust_version="1.99.0",
        )

        with self.assertRaisesRegex(ValueError, "rust-version"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )

    def test_rejects_inexact_internal_requirement(self) -> None:
        archives = self.all_archives()
        umbrella = self.make_archive(
            "rust-composable-architecture",
            dependencies='rca-core = { version = "0.9" }',
        )
        archives = [
            umbrella if path.name.startswith("rust-composable-architecture-") else path
            for path in archives
        ]

        with self.assertRaisesRegex(ValueError, "exact RCA dependency"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )

    def test_rejects_missing_packaged_readme(self) -> None:
        archives = self.all_archives()
        core = self.make_archive("rca-core", include_readme=False)
        archives = [core if path.name.startswith("rca-core-") else path for path in archives]

        with self.assertRaisesRegex(ValueError, "packaged readme"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )

    def test_rejects_featureless_tokio_archive(self) -> None:
        archives = self.all_archives()
        tokio = self.make_archive("rca-tokio", features={})
        archives = [tokio if path.name.startswith("rca-tokio-") else path for path in archives]

        with self.assertRaisesRegex(ValueError, "feature contract"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )

    def test_rejects_wrong_umbrella_feature_wiring(self) -> None:
        archives = self.all_archives()
        features = dict(EXPECTED_FEATURES["rust-composable-architecture"])
        features["tokio"] = ["dep:rca-test"]
        umbrella = self.make_archive(
            "rust-composable-architecture", features=features
        )
        archives = [
            umbrella if path.name.startswith("rust-composable-architecture-") else path
            for path in archives
        ]

        with self.assertRaisesRegex(ValueError, "feature contract"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )

    def test_rejects_missing_packaged_license(self) -> None:
        archives = self.all_archives()
        core = self.make_archive("rca-core", include_apache_license=False)
        archives = [core if path.name.startswith("rca-core-") else path for path in archives]

        with self.assertRaisesRegex(ValueError, "LICENSE-APACHE.md"):
            package_registry.stage_archives(
                self.index,
                self.downloads,
                archives,
                "0.1.0",
                "1.98.0",
            )


class RegistrySourceValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        self.helper = pathlib.Path(package_registry.__file__)

    def run_validation(self, source: str | None) -> subprocess.CompletedProcess[str]:
        metadata = self.root / "metadata.json"
        metadata.write_text(
            json.dumps(
                {
                    "packages": [
                        {
                            "name": "rca-macros",
                            "version": "0.1.0",
                            "source": source,
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        return subprocess.run(
            [
                sys.executable,
                str(self.helper),
                "validate-registry-source",
                "--metadata",
                str(metadata),
                "--package",
                "rca-macros",
                "--registry-index",
                "file:///tmp/rca-index",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_accepts_exact_temporary_registry_source(self) -> None:
        result = self.run_validation("registry+file:///tmp/rca-index")

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_workspace_path_source(self) -> None:
        result = self.run_validation(None)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not resolved from the temporary registry", result.stderr)

    def consumer_metadata(self, extra: list[dict[str, object]] | None = None) -> pathlib.Path:
        metadata = self.root / "consumer.json"
        metadata.write_text(json.dumps({"packages": [
            {"name": "rca-minimal-facade-downstream", "source": None, "version": "0.0.0"},
            {"name": "rust-composable-architecture", "version": "0.1.0", "source": "registry+file:///tmp/rca-index"},
            {"name": "rca-core", "version": "0.1.0", "source": "registry+file:///tmp/rca-index"},
            *(extra or []),
        ]}), encoding="utf-8")
        return metadata

    def test_consumer_accepts_only_exact_local_rca_sources(self) -> None:
        package_registry.validate_consumer_source(self.consumer_metadata(), "0.1.0", "file:///tmp/rca-index")

    def test_consumer_rejects_optional_dependencies_in_minimal_facade(self) -> None:
        for name in ("rca-persistence", "rca-test", "rca-macros", "tokio", "tempfile", "insta"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "minimal consumer acquired"):
                package_registry.validate_consumer_source(self.consumer_metadata([
                    {"name": name, "version": "0.1.0", "source": "registry+file:///tmp/rca-index"}
                ]), "0.1.0", "file:///tmp/rca-index")

    def test_consumer_rejects_transitive_path_source(self) -> None:
        metadata = self.consumer_metadata()
        contents = json.loads(metadata.read_text())
        contents["packages"][2]["source"] = None
        metadata.write_text(json.dumps(contents))
        with self.assertRaisesRegex(ValueError, "not resolved from the temporary registry"):
            package_registry.validate_consumer_source(metadata, "0.1.0", "file:///tmp/rca-index")

    def test_generic_persistence_rejects_runtime_file_and_snapshot_dependencies(self) -> None:
        for name in ("tokio", "tempfile", "insta"):
            metadata = self.consumer_metadata([
                {"name": name, "version": "1.0.0", "source": "registry+https://github.com/rust-lang/crates.io-index"}
            ])
            contents = json.loads(metadata.read_text())
            contents["packages"][0]["name"] = "rca-persistence-downstream"
            metadata.write_text(json.dumps(contents))
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "generic persistence consumer acquired"):
                package_registry.validate_consumer_source(metadata, "0.1.0", "file:///tmp/rca-index")

    def test_consumer_rejects_transitive_inexact_version(self) -> None:
        metadata = self.consumer_metadata()
        contents = json.loads(metadata.read_text())
        contents["packages"][2]["version"] = "0.2.0"
        metadata.write_text(json.dumps(contents))
        with self.assertRaisesRegex(ValueError, "unexpected RCA version"):
            package_registry.validate_consumer_source(metadata, "0.1.0", "file:///tmp/rca-index")

    def test_consumer_checks_transport_sources_and_versions(self) -> None:
        for name in ("rca-event-listener", "rca-event-listener-strategy", "rca-async-channel"):
            for source, version, error in (
                (None, "0.1.0", "not resolved from the temporary registry"),
                ("registry+https://github.com/rust-lang/crates.io-index", "0.1.0", "not resolved from the temporary registry"),
                ("registry+file:///tmp/rca-index", "0.2.0", "unexpected RCA version"),
            ):
                with self.subTest(name=name, source=source, version=version):
                    metadata = self.consumer_metadata([{"name": name, "version": version, "source": source}])
                    with self.assertRaisesRegex(ValueError, error):
                        package_registry.validate_consumer_source(metadata, "0.1.0", "file:///tmp/rca-index")


class ReleaseBoundarySourceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.repo_root = pathlib.Path(__file__).resolve().parents[2]

    def run_registry_commit(self, root: pathlib.Path, ambient: dict[str, str], signer_failure: bool = False, core_resolution: bool = False, complete_run: bool = False, shell: str = "bash") -> subprocess.CompletedProcess[str]:
        # Only the expensive archive/Cargo boundaries are doubled. The runner's
        # actual Git commit, signer provisioning and verification remain real.
        tools = root / "tools"
        tools.mkdir()
        (tools / "rustc").write_text("#!/bin/sh\necho 'rustc 1.98.0'\n")
        (tools / "cargo").write_text(f"#!{sys.executable}\n" + '''import json, os, pathlib, sys
if sys.argv[1] == "--version":
    print("cargo 1.98.0")
else:
    root = pathlib.Path(os.environ["REGISTRY_CAPTURE"])
    (root / "packaging-started").touch()
    with (root / "cargo-commands").open("a") as log:
        print(" ".join(sys.argv[1:]), file=log)
    with (root / "cargo-argv").open("a") as log:
        print(json.dumps(sys.argv[1:]), file=log)
    failure = os.environ.get("REGISTRY_FAIL_STEP")
    if failure == "clippy" and sys.argv[1] == "clippy":
        print("consumer lint failed", file=sys.stderr)
        sys.exit(76)
    if failure == "release" and sys.argv[1] == "run" and "--release" in sys.argv:
        print("release consumer failed", file=sys.stderr)
        sys.exit(77)
    if sys.argv[1] == "metadata" and os.environ["REGISTRY_COMPLETE_RUN"] != "1":
        sys.exit(74)
''')
        if signer_failure:
            (tools / "ssh-keygen").write_text("#!/bin/sh\necho 'signer unavailable' >&2\nexit 42\n")
        (tools / "python3").write_text('''#!/usr/bin/env bash
set -euo pipefail
if [[ $1 == */readme_quickstart.py ]]; then
    printf '%s\\0' "$@" >> "${REGISTRY_CAPTURE}/readme-argv"
    exec "${REGISTRY_REAL_PYTHON}" "$@"
fi
case "$2" in
    validate-fixtures) exit 0 ;;
    validate-registry-source|validate-consumer-source) exit 0 ;;
    stage-archives)
        index=$4
        printf '%s\\n' "${@:11}" > "${REGISTRY_CAPTURE}/staged-archives"
        printf '{"dl":"file:///unused/{crate}.crate"}\\n' > "${index}/config.json"
        if [[ ${REGISTRY_CORE_RESOLUTION} == 1 || ${REGISTRY_COMPLETE_RUN} == 1 ]]; then
            "${REGISTRY_REAL_PYTHON}" -c 'import sys, tarfile; tarfile.open(sys.argv[1], "w:gz").close()' "$6/rca-core-0.1.0.crate"
        fi
        exit 0 ;;
    write-config)
        index=$4
        cp -R "${index}" "${REGISTRY_CAPTURE}/index"
        cp "${index%/*}/signing/allowed-signers" "${REGISTRY_CAPTURE}/allowed-signers"
        if [[ -f ${index%/*}/signing/key ]]; then
            echo 'private signer survived verification' >&2
            exit 52
        fi
        if [[ ${REGISTRY_CORE_RESOLUTION} == 1 || ${REGISTRY_COMPLETE_RUN} == 1 ]]; then
            exec "${REGISTRY_REAL_PYTHON}" "$@"
        fi
        exit 73 ;;
esac
exit 53
''')
        for tool in tools.iterdir():
            tool.chmod(0o755)
        return subprocess.run(
            [shell, str(self.repo_root / "scripts/verify-packaged-downstream.sh")],
            env={**os.environ, "RUSTC": str(tools / "rustc"),
                 "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                 "GIT_CONFIG_COUNT": "0", "REGISTRY_CAPTURE": str(root),
                 "REGISTRY_CORE_RESOLUTION": "1" if core_resolution else "0",
                 "REGISTRY_COMPLETE_RUN": "1" if complete_run else "0",
                 "REGISTRY_REAL_PYTHON": sys.executable, **ambient},
            cwd=self.repo_root, capture_output=True, text=True, check=False,
        )

    def assert_registry_signature(self, ambient: dict[str, str]) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            result = self.run_registry_commit(root, ambient)
            self.assertEqual(result.returncode, 73, result.stdout + result.stderr)
            verified = subprocess.run(
                ["git", "-C", str(root / "index"), "-c", "gpg.format=ssh",
                 "-c", f"gpg.ssh.allowedSignersFile={root / 'allowed-signers'}",
                 "verify-commit", "HEAD"],
                env={**os.environ, "GIT_CONFIG_NOSYSTEM": "1",
                     "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_COUNT": "0"},
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(verified.returncode, 0, verified.stderr)
            self.assertIn('Good "git" signature', verified.stderr)
            self.assertNotIn("PRIVATE KEY", (root / "allowed-signers").read_text())

    def test_registry_commit_without_personal_git_configuration(self) -> None:
        # Removing self-contained signing must fail before the captured real commit.
        self.assert_registry_signature({})

    def test_registry_commit_overrides_invalid_ambient_signing_settings(self) -> None:
        self.assert_registry_signature({
            "GIT_CONFIG_COUNT": "3", "GIT_CONFIG_KEY_0": "gpg.format",
            "GIT_CONFIG_VALUE_0": "invalid", "GIT_CONFIG_KEY_1": "user.signingkey",
            "GIT_CONFIG_VALUE_1": "/unavailable/personal-key",
            "GIT_CONFIG_KEY_2": "gpg.ssh.program",
            "GIT_CONFIG_VALUE_2": "/unavailable/personal-signer",
        })

    def test_registry_signer_failure_precedes_expensive_packaging(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            result = self.run_registry_commit(root, {}, signer_failure=True)
            self.assertEqual(result.returncode, 42, result.stderr)
            self.assertFalse((root / "packaging-started").exists())

    def test_runner_packages_owned_transport_closure_before_consumers(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            result = self.run_registry_commit(root, {})
            self.assertEqual(result.returncode, 73, result.stdout + result.stderr)
            commands = (root / "cargo-commands").read_text().splitlines()
            names = [command.split("--package ", 1)[1].split()[0] for command in commands]
            self.assertEqual(set(names), {
                "rca-event-listener", "rca-event-listener-strategy", "rca-async-channel",
                "rca-core", "rca-macros", "rca-persistence", "rca-test", "rca-tokio",
                "rust-composable-architecture",
            })
            self.assertEqual(len(names), 9)
            staged = (root / "staged-archives").read_text().splitlines()
            self.assertEqual({pathlib.Path(path).name for path in staged},
                             {f"{name}-0.1.0.crate" for name in names})
            self.assertEqual(len(staged), 9)
            self.assertLess(names.index("rca-event-listener"), names.index("rca-event-listener-strategy"))
            self.assertLess(names.index("rca-event-listener-strategy"), names.index("rca-async-channel"))
            self.assertLess(names.index("rca-async-channel"), names.index("rca-core"))
            for dependency, consumer in (("rca-macros", "rca-core"), ("rca-core", "rca-test"),
                                         ("rca-core", "rca-persistence"), ("rca-test", "rca-tokio"),
                                         ("rca-tokio", "rust-composable-architecture")):
                self.assertLess(names.index(dependency), names.index(consumer))
            for command in commands:
                for transport in ("rca-event-listener", "rca-event-listener-strategy", "rca-async-channel"):
                    self.assertIn(f'patch.crates-io.{transport}.path="dependencies/{transport}"', command)

    def test_packaged_core_resolution_uses_transport_registry_not_paths(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            result = self.run_registry_commit(root, {}, core_resolution=True)
            self.assertEqual(result.returncode, 74, result.stdout + result.stderr)
            command = (root / "cargo-commands").read_text().splitlines()[-1]
            self.assertTrue(command.startswith("metadata "), command)
            self.assertNotIn(".path=", command)
            for name in ("rca-macros", "rca-event-listener", "rca-event-listener-strategy", "rca-async-channel"):
                self.assertIn(f'patch.crates-io.{name}.registry="rca-local"', command)
                self.assertIn(f'patch.crates-io.{name}.version="=0.1.0"', command)

    def test_packaging_supplies_macro_dependency_for_test_archive(self) -> None:
        # This double checks the Cargo command boundary, not source text: package
        # resolution must receive the new unpublished development dependency.
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            cargo = root / "cargo"
            cargo.write_text('''#!/usr/bin/env bash
if [[ $1 == --version ]]; then echo 'cargo 1.98.0'; exit 0; fi
if [[ "$*" == *"--package rca-test"* ]]; then
    if [[ "$*" == *'patch.crates-io.rca-macros.path="rca-macros"'* ]]; then
        exit 73
    fi
    echo 'missing unpublished macro dependency patch' >&2
    exit 51
fi
exit 0
''')
            rustc = root / "rustc"
            rustc.write_text("#!/bin/sh\necho 'rustc 1.98.0'\n")
            cargo.chmod(0o755)
            rustc.chmod(0o755)
            result = subprocess.run(["bash", str(self.repo_root / "scripts/verify-packaged-downstream.sh")],
                env={**os.environ, "RUSTC": str(rustc)}, cwd=self.repo_root,
                capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 73, result.stderr)

    def test_packaging_supplies_test_dependency_for_macro_archive(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            result = self.run_registry_commit(root, {})
            self.assertEqual(result.returncode, 73, result.stdout + result.stderr)
            commands = (root / "cargo-commands").read_text().splitlines()
            command = next(command for command in commands if "--package rca-macros " in command)
            self.assertIn('patch.crates-io.rca-test.path="rca-test"', command)

    def test_runner_preserves_readme_and_consumer_arguments_under_bash(self) -> None:
        fixtures = (
            "downstream-packaged", "downstream-composition", "downstream-minimal-core",
            "downstream-minimal-facade", "downstream-ecosystem", "downstream-persistence",
            "downstream-file", "packaged-examples/counter", "packaged-examples/search",
            "packaged-examples/timer", "packaged-examples/composition-search",
            "readme-quickstart", "readme-keyed",
        )
        flags = {
            "generate-lockfile": [], "fetch": ["--locked"],
            "metadata": ["--locked", "--offline"], "check": ["--locked", "--offline"],
            "test": ["--locked", "--offline"], "run": ["--locked", "--offline"],
            "clippy": ["--all-targets", "--locked", "--offline"],
        }
        for shell in ("/bin/bash", "bash"):
            with self.subTest(shell=shell), tempfile.TemporaryDirectory(prefix="rca shell test ") as directory:
                root = pathlib.Path(directory)
                result = self.run_registry_commit(root, {"TMPDIR": str(root)}, complete_run=True, shell=shell)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn("qualification passed: 9 archives, 13 consumers", result.stdout,
                              result.stdout + result.stderr)
                readme_args = (root / "readme-argv").read_bytes().decode().split("\0")[:-1]
                consumers = pathlib.Path(readme_args[2]).parent
                self.assertEqual(readme_args, [
                    str(self.repo_root / "scripts/readme_quickstart.py"),
                    str(self.repo_root / "README.md"), str(consumers / "readme-quickstart"),
                    str(self.repo_root / "scripts/readme_quickstart.py"),
                    str(self.repo_root / "README.md"), str(consumers / "readme-keyed"), "--keyed",
                ])
                commands = [json.loads(line) for line in (root / "cargo-argv").read_text().splitlines()]
                for fixture in fixtures:
                    manifest = str(consumers / fixture / "Cargo.toml")
                    for command, options in flags.items():
                        matching = [args for args in commands if args[0] == command and manifest in args]
                        expected = [command]
                        if command == "metadata":
                            expected += ["--format-version", "1"]
                        expected += ["--manifest-path", manifest, *options]
                        if fixture.startswith("readme-"):
                            expected += [
                                "--config", 'patch.crates-io.rust-composable-architecture.registry="rca-local"',
                                "--config", 'patch.crates-io.rust-composable-architecture.version="=0.1.0"',
                            ]
                        if command == "clippy":
                            expected += ["--", "-D", "warnings"]
                        expected_commands = [expected]
                        if command == "run" and fixture == "downstream-ecosystem":
                            expected_commands += [["run", "--release", *expected[1:]]]
                        with self.subTest(shell=shell, fixture=fixture, command=command):
                            self.assertEqual(matching, expected_commands)
                self.assertFalse(consumers.parent.exists())

    def test_runner_propagates_consumer_lint_and_release_failures_under_bash(self) -> None:
        # Skipping either required command or ignoring its status must not turn
        # an incomplete qualification into success or retain its temporary root.
        for shell in ("/bin/bash", "bash"):
            for step, status, message in (
                ("clippy", 76, "consumer lint failed"),
                ("release", 77, "release consumer failed"),
            ):
                with self.subTest(shell=shell, step=step), tempfile.TemporaryDirectory(prefix="rca failure test ") as directory:
                    root = pathlib.Path(directory)
                    result = self.run_registry_commit(
                        root, {"TMPDIR": str(root), "REGISTRY_FAIL_STEP": step},
                        complete_run=True, shell=shell,
                    )
                    self.assertEqual(result.returncode, status, result.stdout + result.stderr)
                    self.assertIn(message, result.stderr)
                    self.assertNotIn("qualification passed", result.stdout)
                    commands = [json.loads(line) for line in (root / "cargo-argv").read_text().splitlines()]
                    failed = commands[-1]
                    self.assertEqual(failed[0], "clippy" if step == "clippy" else "run")
                    if step == "release":
                        self.assertIn("--release", failed)
                    manifest = pathlib.Path(failed[failed.index("--manifest-path") + 1])
                    self.assertEqual(manifest.parent.name,
                                     "downstream-packaged" if step == "clippy" else "downstream-ecosystem")
                    self.assertFalse(manifest.parents[2].exists())

    def test_cleanup_preserves_failure_and_rejects_incomplete_success_under_bash(self) -> None:
        script = (self.repo_root / "scripts/verify-packaged-downstream.sh").read_text()
        cleanup = script.split("qualification_parent=", 1)[1].split("# The disposable signer", 1)[0]
        cases = (
            ("success", "qualification_completed=true; exit 0", 0, 0),
            ("command failure", "exit 37", 0, 37),
            ("failure and cleanup failure", "exit 37", 41, 37),
            ("success and cleanup failure", "qualification_completed=true; exit 0", 41, 41),
            ("incomplete success", "exit 0", 0, 1),
            ("fatal expansion", 'printf "%s\\n" "$missing_qualification_value"', 0, 1),
            ("unexpected target", 'qualification_root="$TEST_ROOT/unrelated"; exit 37', 0, 37),
        )
        for shell in ("/bin/bash", "bash"):
            for name, ending, removal_status, expected in cases:
                with self.subTest(shell=shell, case=name), tempfile.TemporaryDirectory(prefix="rca cleanup test ") as directory:
                    root = pathlib.Path(directory)
                    unrelated = root / "unrelated"
                    unrelated.mkdir()
                    body = '''set -euo pipefail
rm() {
    printf '%s\\0' "$@" > "$TEST_ROOT/removal-argv"
    if (( REMOVAL_STATUS != 0 )); then return "$REMOVAL_STATUS"; fi
    command rm "$@"
}
qualification_parent=''' + cleanup + '''
printf '%s\\n' "$qualification_root" > "$TEST_ROOT/allocated-root"
''' + ending
                    result = subprocess.run([shell, "-c", body],
                        env={**os.environ, "TMPDIR": str(root), "TEST_ROOT": str(root),
                             "REMOVAL_STATUS": str(removal_status)},
                        capture_output=True, text=True, check=False)
                    self.assertEqual(result.returncode, expected, result.stdout + result.stderr)
                    allocated = pathlib.Path((root / "allocated-root").read_text().strip())
                    self.assertTrue(unrelated.is_dir())
                    if name == "unexpected target":
                        self.assertFalse((root / "removal-argv").exists())
                        self.assertIn("refusing to remove unexpected path", result.stderr)
                    else:
                        self.assertEqual((root / "removal-argv").read_bytes().decode().split("\0")[:-1],
                                         ["-rf", "--", str(allocated)])
                        self.assertEqual(allocated.exists(), removal_status != 0)
                        if removal_status:
                            self.assertIn("failed to remove qualification directory", result.stderr)

    def test_post_staging_verification_has_no_workspace_macro_patch(self) -> None:
        script = (self.repo_root / "scripts/verify-packaged-downstream.sh").read_text(
            encoding="utf-8"
        )
        post_staging = script.split("stage-archives", maxsplit=1)[1]

        self.assertNotIn("patch.crates-io.rca-macros.path", post_staging)
        self.assertIn("validate-registry-source", post_staging)

    def test_temporary_cleanup_is_prefix_guarded(self) -> None:
        script = (self.repo_root / "scripts/verify-packaged-downstream.sh").read_text(
            encoding="utf-8"
        )

        self.assertIn('case "${qualification_root}" in', script)
        self.assertIn(
            '"${qualification_parent}"/rca-package-qualification.*)', script
        )

    def test_tokio_proof_uses_controlled_time(self) -> None:
        source = (
            self.repo_root / "tests/downstream-composition/src/main.rs"
        ).read_text(encoding="utf-8")

        self.assertIn("start_paused = true", source)

    def test_packaged_allocation_runners_match_shared_source(self) -> None:
        source = (
            self.repo_root / "rca-core/src/allocation_test/runner.rs"
        ).read_text(encoding="utf-8")
        copies = (
            "rca-tokio/tests/support/allocation_runner.rs",
            "rca-persistence/src/allocation_runner.rs",
        )
        for path in copies:
            with self.subTest(path=path):
                packaged = (self.repo_root / path).read_text(encoding="utf-8")
                header, separator, body = packaged.partition("\n\n")
                self.assertTrue(header.startswith("// Package-local copy of "))
                self.assertEqual(separator, "\n\n")
                self.assertEqual(body, source)

    def test_tokio_proof_asserts_timer_cancellation(self) -> None:
        source = (
            self.repo_root / "tests/downstream-composition/src/main.rs"
        ).read_text(encoding="utf-8")

        self.assertIn("assert_eq!(active_timers.len(), 1);", source)
        self.assertIn("assert!(cancelled_timers.is_empty());", source)

    def test_legacy_views_are_retired_from_workspace(self) -> None:
        manifest = tomllib.loads(
            (self.repo_root / "Cargo.toml").read_text(encoding="utf-8")
        )

        self.assertNotIn("composable-views", manifest["workspace"]["members"])
        self.assertFalse((self.repo_root / "composable-views/Cargo.toml").exists())
        self.assertFalse((self.repo_root / "composable-views/src/ui_id/Cargo.toml").exists())
        self.assertTrue(
            {"composable-views", "ui_id"}.isdisjoint(package_registry.RCA_PACKAGES)
        )

    def test_support_separates_local_and_hosted_qualification(self) -> None:
        support = (self.repo_root / "docs/support.md").read_text(encoding="utf-8")
        prose = " ".join(support.split())

        self.assertIn("Release qualification covers", prose)
        self.assertIn(
            "Each result applies to the source, dependencies, toolchain and environment it covers.",
            prose,
        )
        self.assertIn(
            "Local results do not establish that hosted checks passed.", prose
        )
        self.assertIn("for the exact commit under review.", prose)
        self.assertIn("Later changes require their own applicable checks.", prose)
        self.assertIn(
            "[CI runs](https://github.com/jacobcxdev/"
            "rust-composable-architecture/actions/workflows/ci.yml)",
            prose,
        )
        self.assertIn(
            "Passing checks do not publish packages or establish support beyond the tested configurations.",
            prose,
        )

        targets = support.split("## Targets\n", 1)[1].split("## Features\n", 1)[0]
        self.assertIn(
            "The table defines required checks, not the result of any particular run.",
            " ".join(targets.split()),
        )
        rows = [
            [cell.strip() for cell in line.strip("|").split("|")]
            for line in targets.splitlines()
            if line.startswith("| `")
        ]
        self.assertEqual(
            rows,
            [
                ["`x86_64-unknown-linux-gnu`", "Required", "Required"],
                ["`aarch64-unknown-linux-gnu`", "Required", "Required"],
                [
                    "`x86_64-apple-darwin`", "Required, compile-only",
                    "Required, compile-only",
                ],
                ["`aarch64-apple-darwin`", "Required", "Required"],
                ["`x86_64-pc-windows-msvc`", "Required", "Required"],
                ["`wasm32-unknown-unknown`", "Required", "Not applicable"],
            ],
        )

    def test_qualification_points_to_existing_tokio_conformance_source(self) -> None:
        qualification = (self.repo_root / "docs/qualification.md").read_text(
            encoding="utf-8"
        )

        self.assertIn("`rca-tokio/src/lib.rs`", qualification)
        self.assertTrue((self.repo_root / "rca-tokio/src/lib.rs").is_file())
        self.assertNotIn("rca-tokio/tests/conformance_tokio.rs", qualification)


if __name__ == "__main__":
    unittest.main()
