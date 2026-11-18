"""Check the opt-in heap counter without accepting a stack profile or a changed build."""

import copy
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_allocation_counts as counts


class AllocationCountTests(unittest.TestCase):
    def test_all_three_totals_must_match_the_single_empty_stack(self):
        row = {"allocations": 6, "allocated_bytes": 448, "peak_bytes": 384}
        profile = {
            "dhatFileVersion": 2,
            "mode": "rust-heap",
            "ftbl": ["[root]"],
            "pps": [{"fs": [], "tbk": 6, "tb": 448, "mb": 384, "gb": 384}],
        }
        counts.check_sample(profile, row)
        for key, value in (("fs", [0]), ("mb", 448), ("tbk", True), ("tb", 0)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                altered = copy.deepcopy(profile)
                altered["pps"][0][key] = value
                counts.check_sample(altered, row)
        for key, value in (("mode", "ad-hoc"), ("ftbl", ["[root]", "frame"]), ("pps", [])):
            with self.subTest(key=key), self.assertRaises(ValueError):
                counts.check_sample({**profile, key: value}, row)

    def test_staging_refuses_unpinned_archives_and_changed_sources(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            source = root / "original"
            source.mkdir()
            (source / "Cargo.toml").write_text("original")
            manifest = root / "inventory.json"
            manifest.write_text(json.dumps(counts.files(source)))
            archive = root / "dhat-0.3.3.crate"
            archive.write_bytes(b"wrong archive")
            target = root / "new"
            for digest in ("0" * 64, counts.digest(manifest)):
                with self.subTest(digest=digest), self.assertRaises(ValueError):
                    counts.stage(source, manifest, digest, archive, target, "native")
                self.assertFalse(target.exists())
            (source / "Cargo.toml").write_text("changed")
            with self.assertRaisesRegex(ValueError, "source"):
                counts.stage(source, manifest, counts.digest(manifest), archive, target, "native")

    def test_existing_destination_remains_untouched(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            (root / "existing").mkdir()
            with self.assertRaises((FileExistsError, ValueError)):
                counts.stage(root, root / "manifest", "0" * 64, root / "crate", root / "existing", "native")
            self.assertTrue((root / "existing").is_dir())

    def test_references_keep_their_own_test_runtime_features(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp).resolve()
            package = source / "rca-test"
            package.mkdir()
            manifest = package / "Cargo.toml"
            for enabled in (["test-util"], ["diagnostics", "test-util"], ["local"]):
                manifest.write_text(
                    "[dependencies]\nrca-core = { workspace = true, features = "
                    + json.dumps(enabled)
                    + " }\n"
                )
                if enabled == ["local"]:
                    with self.assertRaises(ValueError):
                        counts.core_features(source, "deterministic")
                else:
                    self.assertEqual(counts.core_features(source, "deterministic"), ["default", *enabled])

    def test_invalid_lock_cannot_change_the_profiler(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp).resolve()
            (source / "rca-tokio").mkdir()
            (source / "rca-tokio/Cargo.toml").write_text(
                "[features]\nqualification-dhat=[]\n[dev-dependencies]\ndhat.workspace=true"
            )
            (source / "Cargo.toml").write_text('[workspace.dependencies]\ndhat="0.3.3"')
            (source / "Cargo.lock").write_text(
                '[[package]]\nname="dhat"\nversion="0.3.3"\nsource="elsewhere"'
            )
            with self.assertRaisesRegex(ValueError, "lockfile"):
                counts.adapt_inputs(source, source / "unreachable", "native")

    def test_only_the_fresh_expected_compiler_configuration_is_admitted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            source = root / "source"
            dependency = root / counts.CRATE
            cases = (
                ("dhat", dependency, "src/lib.rs", [], "lib"),
                ("rca_core", source / "rca-core", "src/lib.rs", ["default"], "lib"),
                ("rca_tokio", source / "rca-tokio", "src/lib.rs", ["qualification-dhat"], "lib"),
                (
                    "qualification",
                    source / "rca-tokio",
                    "benches/qualification.rs",
                    ["qualification-dhat"],
                    "bench",
                ),
            )
            records = []
            binary, log = root / "qualification", root / "build.jsonl"
            binary.write_bytes(b"compiled fixture")
            for name, package, entry, features, kind in cases:
                src = package / entry
                src.parent.mkdir(parents=True, exist_ok=True)
                src.write_bytes(b"fixture")
                manifest = package / "Cargo.toml"
                manifest.touch()
                records.append(
                    {
                        "reason": "compiler-artifact",
                        "fresh": False,
                        "package_id": "path+" + package.as_uri() + "#0.1.0",
                        "target": {"name": name, "kind": [kind], "src_path": str(src)},
                        "features": features,
                        "manifest_path": str(manifest),
                        "executable": str(binary) if kind == "bench" else None,
                        "filenames": [str(binary)],
                        "profile": {
                            "opt_level": "3",
                            "debuginfo": 2,
                            "debug_assertions": False,
                            "overflow_checks": False,
                            "test": kind == "bench",
                        },
                    }
                )
            receipt = root / "counts-source.json"
            receipt.write_text(
                json.dumps(
                    {
                        "configuration": "native",
                        "archive_sha256": counts.ARCHIVE,
                        "source_tree_sha256": counts.tree_digest(source),
                        "dependency_tree_sha256": counts.tree_digest(dependency),
                    }
                )
            )

            def verify(rows):
                lines = [*rows, {"reason": "build-finished", "success": True}]
                log.write_text("\n".join(map(json.dumps, lines)))
                return counts.check_build(root, counts.digest(receipt), log, binary)

            with mock.patch.object(counts, "PATCHED", hashlib.sha256(b"fixture").hexdigest()):
                self.assertEqual(verify(records), counts.digest(binary))
                # Windows Cargo can mix path separators. Preserve Cargo's spelling in its output list.
                spelled = copy.deepcopy(records)
                decorated = str(root) + "/./qualification"
                spelled[-1].update(executable=decorated, filenames=[decorated])
                self.assertEqual(verify(spelled), counts.digest(binary))
                spelled[-1]["filenames"] = [str(root / "other")]
                with self.assertRaisesRegex(ValueError, "binary"):
                    verify(spelled)
                for i, field, value in (
                    (0, "fresh", True),
                    (0, "package_id", counts.REGISTRY + "#dhat@0.3.3"),
                    (1, "features", ["diagnostics"]),
                    (2, "manifest_path", str(dependency / "Cargo.toml")),
                    (3, "executable", str(root / "other")),
                    (0, "profile", {}),
                ):
                    with self.subTest(field=field), self.assertRaises(ValueError):
                        rows = copy.deepcopy(records)
                        rows[i][field] = value
                        verify(rows)
                with self.assertRaisesRegex(ValueError, "rca_tokio"):
                    verify(records[:2] + records[3:])
                (dependency / "injected.rs").touch()
                with self.assertRaisesRegex(ValueError, "inputs"):
                    verify(records)
