"""Exercise paired gates with real JSON and a controlled Git observation boundary."""

import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
COMPARATOR = ROOT / "scripts/compare-performance.sh"
CANDIDATE = "a" * 40
REFERENCE = "b" * 40


def digest(data):
    return hashlib.sha256(data).hexdigest()


class NativePerformanceComparisonTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="rca-native-comparison-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        inputs = {
            "Cargo.lock": b"candidate lock\n",
            "Cargo.toml": b"manifest\n",
            "docs/performance/baseline-Cargo.lock": b"reference lock\n",
            "docs/performance/baseline-revision.txt": (REFERENCE + "\n").encode(),
            "docs/performance/scenarios.txt": (ROOT / "docs/performance/scenarios.txt").read_bytes(),
            "rca-tokio/benches/qualification.rs": b"harness\n",
            "rca-tokio/benches/qualification/shutdown.rs": b"shutdown harness\n",
            "scripts/qualify-performance.sh": b"Mac capture\n",
            "scripts/guard_cargo_memory.py": b"Mac memory watchdog\n",
            "scripts/capture-windows-performance.py": (ROOT / "scripts/capture-windows-performance.py").read_bytes(),
        }
        for relative, contents in inputs.items():
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(contents)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        git = self.bin / "git"
        git.write_text("""#!/bin/sh
case "$1" in
  rev-parse)
    case "$2" in
      --show-toplevel) printf '%s\\n' "$RCA_TEST_ROOT" ;;
      HEAD) printf '%s\\n' "$RCA_TEST_CANDIDATE" ;;
      *) printf '%s\\n' "$RCA_TEST_TREE" ;;
    esac ;;
  show)
    case "$2" in
      *:Cargo.toml) printf 'manifest\\n' ;;
      *:rca-tokio/Cargo.toml) printf 'package manifest\\n' ;;
      *:rca-tokio/benches/qualification.rs) printf 'harness\\n' ;;
      *) exit 99 ;;
    esac ;;
  cat-file|merge-base|diff|ls-files) exit 0 ;;
  *) exit 99 ;;
esac
""")
        git.chmod(0o755)

    def evidence(self, windows):
        capture = "scripts/capture-windows-performance.py" if windows else "scripts/qualify-performance.sh"
        harness_hash = digest(b"harness\n")
        module_hash = digest(b"shutdown harness\n")
        scenario_hash = digest((self.root / "docs/performance/scenarios.txt").read_bytes())
        capture_hash = digest((self.root / capture).read_bytes())
        cohort = {
            "id": "c" * 64,
            "recorded_at": "2026-10-30T23:00:00Z",
            "runner": "test-windows" if windows else "test-mac",
            "protocol_sha256": digest(f"{harness_hash}\n{module_hash}\n{scenario_hash}\n{capture_hash}\n".encode()),
            "qualification_source_sha256": harness_hash,
            "qualification_module_sha256": module_hash,
            "qualification_script_sha256": capture_hash,
            "scenario_manifest_sha256": scenario_hash,
            "candidate_lock_sha256": digest(b"candidate lock\n"),
            "reference_lock_sha256": digest(b"reference lock\n"),
        }
        if not windows:
            cohort["memory_watchdog_sha256"] = digest((self.root / "scripts/guard_cargo_memory.py").read_bytes())
            cohort["memory_watchdog_receipt"] = str((self.root / "memory.json").resolve())
            cohort["protocol_sha256"] = digest(
                f"{harness_hash}\n{module_hash}\n{scenario_hash}\n{capture_hash}\n{cohort['memory_watchdog_sha256']}\n".encode())
        for role in ("candidate", "reference"):
            cohort[f"{role}_source_tree"] = "d" * 40
            cohort[f"{role}_manifest_sha256"] = digest(b"manifest\n")
            cohort[f"{role}_original_harness_sha256"] = harness_hash
        metadata = {
            "cohort": cohort,
            "method": {
                "aggregation": "geometric mean of order-stratified paired medians",
                "normal_repetitions": 10,
                "allocation_repetitions": 6,
                "pair": {"candidate_revision": CANDIDATE, "reference_revision": REFERENCE},
            },
        }
        if windows:
            metadata["resource_provider"] = "win32-process-v1"
            tools = {"rustc": "rustc 1.100.0 (pinned)",
                     "cargo": "cargo 1.100.0 (pinned)",
                     "rustc_commit": "6" * 40,
                     "target": "x86_64-pc-windows-msvc"}
            for target in (metadata, cohort):
                target.update(compiler=tools["rustc"], cargo=tools["cargo"], rustc_commit=tools["rustc_commit"])
            configuration = "split-native-deterministic-v1"
            source = {"harness_sha256": harness_hash, "harness_module_sha256": module_hash,
                      "scenario_manifest_sha256": scenario_hash,
                      "toolchain": tools, "binaries": {}, "commands": [],
                      "configuration": configuration}
            metadata["binaries"] = {}
            for role, revision in (("candidate", CANDIDATE), ("reference", REFERENCE)):
                deterministic_features = (["default", "diagnostics", "test-util"] if role == "candidate"
                                          else ["default", "test-util"])
                source[role] = {"revision": revision, "tree": cohort[f"{role}_source_tree"],
                                "lock_sha256": cohort[f"{role}_lock_sha256"],
                                "manifest_sha256": cohort[f"{role}_manifest_sha256"],
                                "original_harness_sha256": harness_hash,
                                "tokio_manifest_sha256": digest(b"package manifest\n"),
                                "deterministic_core_features": deterministic_features}
                source[f"{role}_source_manifest_sha256"] = digest(f"{role} original inventory".encode())
                for kind in ("native", "deterministic"):
                    native = kind == "native"
                    for mode in ("normal", "allocation"):
                        key = f"{role}_{'deterministic_' if not native else ''}{mode}"
                        source_root = f"C:/source/{role}-{kind}"
                        binary = {"path": f"C:/capture/{key}.exe", "sha256": digest(key.encode()),
                                  "configuration": kind, "core_features": ["default"] if native else deterministic_features,
                                  "source_root": source_root, "source_manifest": f"{source_root}.json",
                                  "source_manifest_sha256": (digest(f"{role} native inventory".encode()) if native
                                                             else source[f"{role}_source_manifest_sha256"]),
                                  "harness_sha256": digest(b"native harness\n") if native else harness_hash,
                                  "manifest_sha256": (digest(b"native package manifest\n") if native
                                                      else source[role]["tokio_manifest_sha256"]),
                                  "lock_sha256": digest(f"{role} native lock".encode()) if native else source[role]["lock_sha256"]}
                        if native:
                            binary["transformation"] = {"files": {
                                path: {"before_sha256": before, "after_sha256": binary[field]}
                                for path, field, before in (
                                    ("rca-tokio/benches/qualification.rs", "harness_sha256", harness_hash),
                                    ("rca-tokio/Cargo.toml", "manifest_sha256", source[role]["tokio_manifest_sha256"]),
                                    ("Cargo.lock", "lock_sha256", source[role]["lock_sha256"]))}}
                        source["binaries"][key] = binary
                        metadata["binaries"][key] = {field: binary[field] for field in ("path", "sha256")}
                        artifacts = {}
                        for field, name, package, kind_name, crate_type, src, features, test in (
                                ("core_artifact", "rca_core", "rca-core", "lib", "lib", "src/lib.rs", binary["core_features"], False),
                                ("cargo_artifact", "qualification", "rca-tokio", "bench", "bin", "benches/qualification.rs",
                                 ["qualification-dhat"] if mode == "allocation" else [], True)):
                            artifacts[field] = {"reason": "compiler-artifact", "manifest_path": f"{source_root}/{package}/Cargo.toml",
                                                "target": {"name": name, "kind": [kind_name], "crate_types": [crate_type],
                                                           "src_path": f"{source_root}/{package}/{src}"},
                                                "profile": {"opt_level": "3", "debuginfo": 2, "test": test,
                                                            "debug_assertions": False, "overflow_checks": False},
                                                "features": features, "executable": binary["path"] if test else None,
                                                "filenames": [binary["path"]] if test else ["C:/source/core.rlib"]}
                        source["commands"].append({"binary_role": key, "returncode": 0,
                                                   "binary": {"sha256": binary["sha256"]},
                                                   "transcript_sha256": digest(key.encode()), **artifacts})
            metadata["source_build"] = source
            metadata["source_build_sha256"] = digest(json.dumps(source, sort_keys=True, indent=2).encode())
            metadata["configuration"] = cohort["configuration"] = configuration
            cohort["source_build_sha256"] = metadata["source_build_sha256"]
            cohort["protocol_sha256"] = digest(
                f"{harness_hash}\n{module_hash}\n{scenario_hash}\n{capture_hash}\n{metadata['source_build_sha256']}\n".encode())
        rows = []
        for scenario in (self.root / "docs/performance/scenarios.txt").read_text().splitlines():
            allocation_samples = 1000
            if scenario == "deterministic/budget":
                allocation_samples = 5
            elif scenario.endswith("/1000"):
                allocation_samples = 200
            elif scenario.startswith(("mailbox/", "shutdown/")) or scenario.endswith("/64"):
                allocation_samples = 500
            normal = {
                "scenario": scenario, "samples": 10000, "operations": 10000,
                "throughput_per_second": 1000, "latency_ns": {"p50": 100, "p95": 150, "p99": 200},
                "cpu_cycles": 1000, "instructions_retired": None if windows else 1000,
                "task_wakes": 10000,
            }
            if windows:
                normal.update(peak_working_set_bytes=1000, peak_commit_bytes=1000,
                              configuration="deterministic" if scenario.startswith("deterministic/") else "native")
            else:
                normal.update(peak_resident_bytes=1000, peak_memory_footprint_bytes=1000)
            allocation = dict(normal, samples=allocation_samples, operations=allocation_samples,
                              allocations=1000, allocated_bytes=1000, peak_bytes=1000)
            limit = {"shutdown/abortive/live/1": 1, "shutdown/abortive/live/8": 8,
                     "shutdown/abortive/burst/8": 8, "shutdown/abortive/burst/64": 64}.get(scenario)
            if limit is not None:
                for sample in (normal, allocation):
                    count = sample["samples"]
                    batches = count + min(count, 20)
                    sample["operations"] = limit * count
                    sample["shutdown_lifetime"] = dict(
                        live_limit=limit, maximum_unfinished=limit, batches=batches,
                        spawned=limit * batches, dropped=limit * batches,
                        unfinished=0, maximum_batch_end_unfinished=0)
            pairs = {}
            for kind, count, sample in (("normal", 10, normal), ("allocation", 6, allocation)):
                pairs[kind] = []
                for repetition in range(1, count + 1):
                    order = "reference-candidate" if repetition % 2 else "candidate-reference"
                    entry = dict(sample, repetition=repetition, order=order)
                    pairs[kind].append({"repetition": repetition, "order": order,
                                        "candidate": copy.deepcopy(entry), "reference": copy.deepcopy(entry)})
            rows.append(dict(normal, revision=CANDIDATE, runner=cohort["runner"],
                             allocations=1000, allocated_bytes=1000, peak_bytes=1000,
                             allocation_samples=allocation_samples, allocation_operations=allocation["operations"],
                             normal_repetitions=10, allocation_repetitions=6,
                             pairs=pairs, rationale_id=None))
        candidate = dict(schema_version=6 if windows else 4, suite="rca-performance-qualification",
                         reference_kind="qualification", revision=CANDIDATE,
                         runner=cohort["runner"], recorded_at=cohort["recorded_at"],
                         metadata=metadata, accepted_regressions=[], rows=rows)
        reference = copy.deepcopy(candidate)
        reference.update(reference_kind="paired-reference", revision=REFERENCE)
        for row in reference["rows"]:
            row["revision"] = REFERENCE
        return reference, candidate

    def compare(self, reference, candidate, *, guard_change=None, no_guard=False, alter_output=False):
        reference_path = self.root / "reference.json"
        candidate_path = self.root / "candidate.json"
        reference_path.write_text(json.dumps(reference))
        candidate_path.write_text(json.dumps(candidate))
        if candidate["schema_version"] == 4:
            receipt = dict(
                entrypoint="qualify-performance", toolchain="1.100.0", started=True,
                protection="polling-watchdog-not-kernel-memory-cap", reason="command-exited",
                returncode=0, command_returncode=0, cleanup_complete=True, descendants_complete=True,
                forced_cleanup=False, memory_limit_bytes=8 * 1024**3,
                host_reserve_bytes=4 * 1024**3, sample_interval_seconds=0.25,
                peak_footprint_bytes=100, peak_process_count=1, elapsed_seconds=1,
                memory_watchdog_sha256=digest((self.root / "scripts/guard_cargo_memory.py").read_bytes()),
                qualification_script_sha256=digest((self.root / "scripts/qualify-performance.sh").read_bytes()),
                qualification_outputs={role: dict(path=str(path.resolve()), sha256=digest(path.read_bytes()))
                                       for role, path in (("candidate", candidate_path), ("reference", reference_path))},
            )
            if guard_change is not None:
                guard_change(receipt)
            receipt_path = self.root / "memory.json"
            if no_guard:
                receipt_path.unlink(missing_ok=True)
            else:
                receipt_path.write_text(json.dumps(receipt))
            if alter_output:
                candidate_path.write_text(candidate_path.read_text() + "\n")
        if "source_build" in reference["metadata"]:
            (self.root / "source-build.json").write_text(
                json.dumps(reference["metadata"]["source_build"], sort_keys=True, indent=2))
        environment = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                           RCA_TEST_ROOT=str(self.root), RCA_TEST_CANDIDATE=CANDIDATE,
                           RCA_TEST_TREE="d" * 40)
        return subprocess.run(["bash", str(COMPARATOR), str(reference_path), str(candidate_path)],
                              cwd=self.root, env=environment, text=True, capture_output=True, check=False)

    def test_mac_evidence_keeps_its_existing_counter_contract(self):
        result = self.compare(*self.evidence(False))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_windows_uses_explicit_memory_counters_without_fake_instructions(self):
        result = self.compare(*self.evidence(True))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_windows_allows_hash_identical_copies_of_recorded_binaries(self):
        reference, candidate = self.evidence(True)
        for document in (reference, candidate):
            for key, binary in document["metadata"]["binaries"].items():
                binary["path"] = f"C:/retained/{key}.exe"
        result = self.compare(reference, candidate)
        self.assertEqual(result.returncode, 0, result.stderr)

    def rebind_windows_source(self, reference, candidate, source):
        """Keep receipt hashes consistent when testing a deeper invalid build field."""
        for document in (reference, candidate):
            metadata = document["metadata"]
            metadata["source_build"] = copy.deepcopy(source)
            receipt_hash = digest(json.dumps(source, sort_keys=True, indent=2).encode())
            metadata["source_build_sha256"] = metadata["cohort"]["source_build_sha256"] = receipt_hash
            cohort = metadata["cohort"]
            cohort["protocol_sha256"] = digest((cohort["qualification_source_sha256"] + "\n"
                                                + cohort["qualification_module_sha256"] + "\n"
                                                + cohort["scenario_manifest_sha256"] + "\n"
                                                + cohort["qualification_script_sha256"] + "\n"
                                                + receipt_hash + "\n").encode())

    def test_windows_rejects_wrong_features_roles_and_effective_sources(self):
        for mutation in ("native-features", "core-artifact", "missing-role", "same-roots",
                         "transformation", "normal-allocation", "root-manifest"):
            with self.subTest(mutation=mutation):
                reference, candidate = self.evidence(True)
                source = candidate["metadata"]["source_build"]
                binary = source["binaries"]["candidate_normal"]
                if mutation == "native-features":
                    binary["core_features"] = ["default", "test-util"]
                elif mutation == "core-artifact":
                    source["commands"][0]["core_artifact"]["features"] = ["default", "diagnostics"]
                elif mutation == "missing-role":
                    del source["binaries"]["reference_deterministic_normal"]
                elif mutation == "same-roots":
                    binary["source_root"] = source["binaries"]["reference_normal"]["source_root"]
                elif mutation == "transformation":
                    binary["transformation"]["files"]["rca-core/src/store/actor.rs"] = {
                        "before_sha256": "a" * 64, "after_sha256": "b" * 64}
                elif mutation == "normal-allocation":
                    source["binaries"]["candidate_allocation"]["harness_sha256"] = "0" * 64
                else:
                    source["candidate"]["manifest_sha256"] = "0" * 64
                self.rebind_windows_source(reference, candidate, source)
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_windows_requires_configuration_on_every_row_and_observation(self):
        for mutation in ("row", "normal-candidate", "allocation-reference", "deterministic-row", "cohort", "schema"):
            with self.subTest(mutation=mutation):
                reference, candidate = self.evidence(True)
                for document in (reference, candidate):
                    row = document["rows"][0]
                    if mutation == "row":
                        row["configuration"] = "deterministic"
                    elif mutation == "normal-candidate":
                        row["pairs"]["normal"][0]["candidate"]["configuration"] = "deterministic"
                    elif mutation == "allocation-reference":
                        row["pairs"]["allocation"][0]["reference"]["configuration"] = "deterministic"
                    elif mutation == "deterministic-row":
                        next(row for row in document["rows"] if row["scenario"].startswith("deterministic/"))["configuration"] = "native"
                    elif mutation == "cohort":
                        document["metadata"]["cohort"]["configuration"] = "combined"
                    else:
                        document["schema_version"] = 5
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_missing_changed_or_historical_watchdog_provenance(self):
        for mutation in ("missing-candidate", "missing-reference", "changed-hash", "historical-tuple"):
            with self.subTest(mutation=mutation):
                reference, candidate = self.evidence(False)
                if mutation.startswith("missing"):
                    target = candidate if mutation == "missing-candidate" else reference
                    del target["metadata"]["cohort"]["memory_watchdog_sha256"]
                elif mutation == "changed-hash":
                    candidate["metadata"]["cohort"]["memory_watchdog_sha256"] = "0" * 64
                else:
                    for document in (reference, candidate):
                        cohort = document["metadata"]["cohort"]
                        del cohort["memory_watchdog_sha256"]
                        cohort["protocol_sha256"] = digest((
                            cohort["qualification_source_sha256"] + "\n" +
                            cohort["qualification_module_sha256"] + "\n" +
                            cohort["scenario_manifest_sha256"] + "\n" +
                            cohort["qualification_script_sha256"] + "\n").encode())
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_watchdog_source_changed_after_capture(self):
        reference, candidate = self.evidence(False)
        (self.root / "scripts/guard_cargo_memory.py").write_text("changed watchdog\n")
        result = self.compare(reference, candidate)
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_both_platforms_reject_changed_or_missing_shutdown_module_provenance(self):
        for windows in (False, True):
            for mutation in ("missing", "different"):
                with self.subTest(windows=windows, mutation=mutation):
                    reference, candidate = self.evidence(windows)
                    for document in (reference, candidate):
                        cohort = document["metadata"]["cohort"]
                        if mutation == "missing":
                            del cohort["qualification_module_sha256"]
                        else:
                            cohort["qualification_module_sha256"] = "0" * 64
                    result = self.compare(reference, candidate)
                    self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_missing_outer_guard_completion(self):
        result = self.compare(*self.evidence(False), no_guard=True)
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_failed_or_forged_outer_guard_completion(self):
        changes = (
            lambda receipt: receipt.update(returncode=125, reason="descendants-still-running",
                                           descendants_complete=False, forced_cleanup=True),
            lambda receipt: receipt.update(cleanup_complete=False),
            lambda receipt: receipt.update(returncode=False),
            lambda receipt: receipt.update(command_returncode=1),
            lambda receipt: receipt.update(forced_cleanup=True),
            lambda receipt: receipt.update(memory_limit_bytes=16 * 1024**3),
            lambda receipt: receipt.update(memory_watchdog_sha256="0" * 64),
            lambda receipt: receipt["qualification_outputs"]["reference"].update(sha256="0" * 64),
            lambda receipt: receipt["qualification_outputs"]["candidate"].update(path="/unrelated.json"),
        )
        for change in changes:
            with self.subTest(change=change):
                result = self.compare(*self.evidence(False), guard_change=change)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_output_changed_after_final_guard_binding(self):
        result = self.compare(*self.evidence(False), alter_output=True)
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_mismatched_receipt_paths_between_outputs(self):
        reference, candidate = self.evidence(False)
        reference["metadata"]["cohort"]["memory_watchdog_receipt"] = str(self.root / "another.json")
        result = self.compare(reference, candidate)
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_each_windows_resource_and_latency_gate_rejects_an_eleven_percent_regression(self):
        cases = (("normal", "cpu_cycles"), ("normal", "task_wakes"),
                 ("normal", "peak_working_set_bytes"), ("normal", "peak_commit_bytes"),
                 ("allocation", "allocations"), ("allocation", "allocated_bytes"),
                 ("normal", "p50"), ("normal", "p99"), ("normal", "throughput_per_second"))
        for kind, metric in cases:
            with self.subTest(metric=metric):
                reference, candidate = self.evidence(True)
                for pair in candidate["rows"][0]["pairs"][kind]:
                    sample = pair["candidate"]
                    if metric in ("p50", "p99"):
                        sample["latency_ns"][metric] *= 1.11
                    elif metric == "throughput_per_second":
                        sample[metric] /= 1.11
                    else:
                        sample[metric] *= 1.11
                reference["rows"][0]["pairs"] = copy.deepcopy(candidate["rows"][0]["pairs"])
                result = self.compare(reference, candidate)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("unexplained performance regressions", result.stderr)

    def test_bounded_shutdown_gates_only_process_memory_and_retains_other_ratios(self):
        for windows in (False, True):
            for scenario in ("shutdown/abortive/live/1", "shutdown/abortive/live/8",
                             "shutdown/abortive/burst/8", "shutdown/abortive/burst/64"):
                with self.subTest(windows=windows, scenario=scenario):
                    reference, candidate = self.evidence(windows)
                    row = next(row for row in candidate["rows"] if row["scenario"] == scenario)
                    for pair in row["pairs"]["normal"]:
                        pair["candidate"]["latency_ns"]["p50"] = 111
                        pair["candidate"]["latency_ns"]["p99"] = 222
                        pair["candidate"]["throughput_per_second"] = 500
                        pair["candidate"]["cpu_cycles"] = 1110
                        pair["candidate"]["task_wakes"] = 11100
                    for pair in row["pairs"]["allocation"]:
                        pair["candidate"]["allocations"] = 1110
                        pair["candidate"]["allocated_bytes"] = 1110
                    next(item for item in reference["rows"] if item["scenario"] == scenario)["pairs"] = copy.deepcopy(row["pairs"])
                    result = self.compare(reference, candidate)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    report = json.loads((self.root / "candidate-comparison.json").read_text())
                    observed = next(item for item in report["comparisons"] if item["scenario"] == scenario)
                    self.assertEqual(observed["gated_metrics"], ["peak_memory_ratio", "resident_memory_ratio"])
                    self.assertGreater(observed["median_ratio"], 1.10)
                    self.assertEqual(observed["regressed_metrics"], [])

    def test_bounded_shutdown_rejects_eleven_percent_memory_regressions(self):
        for windows, metrics in ((False, ("peak_resident_bytes", "peak_memory_footprint_bytes")),
                                 (True, ("peak_working_set_bytes", "peak_commit_bytes"))):
            for metric in metrics:
                with self.subTest(windows=windows, metric=metric):
                    reference, candidate = self.evidence(windows)
                    row = next(row for row in candidate["rows"] if row["scenario"] == "shutdown/abortive/burst/64")
                    for pair in row["pairs"]["normal"]:
                        pair["candidate"][metric] = 1110
                    next(item for item in reference["rows"] if item["scenario"] == row["scenario"])["pairs"] = copy.deepcopy(row["pairs"])
                    result = self.compare(reference, candidate)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn("unexplained performance regressions", result.stderr)

    def test_original_abortive_memory_is_diagnostic_but_speed_still_gates(self):
        for speed_regression in (False, True):
            reference, candidate = self.evidence(True)
            row = next(row for row in candidate["rows"] if row["scenario"] == "shutdown/abortive")
            for pair in row["pairs"]["normal"]:
                pair["candidate"]["peak_working_set_bytes"] = 2000
                pair["candidate"]["peak_commit_bytes"] = 2000
                if speed_regression:
                    pair["candidate"]["latency_ns"]["p50"] = 111
            next(item for item in reference["rows"] if item["scenario"] == row["scenario"])["pairs"] = copy.deepcopy(row["pairs"])
            result = self.compare(reference, candidate)
            self.assertEqual(result.returncode, int(speed_regression), result.stderr)
            report = json.loads((self.root / "candidate-comparison.json").read_text())
            observed = next(item for item in report["comparisons"] if item["scenario"] == row["scenario"])
            self.assertEqual(observed["peak_memory_ratio"], 2)
            self.assertEqual(observed["resident_memory_ratio"], 2)
            self.assertNotIn("peak_memory_ratio", observed["gated_metrics"])
            self.assertEqual(observed["regressed_metrics"], ["median_ratio"] if speed_regression else [])

    def test_shutdown_lifetime_cannot_be_missing_forged_or_attached_to_legacy_rows(self):
        for mutation in ("missing-row", "missing-normal", "missing-allocation", "batch-leak", "final-leak",
                         "wrong-limit", "wrong-maximum", "wrong-count", "wrong-batches", "boolean",
                         "extra-field", "wrong-operations", "legacy"):
            with self.subTest(mutation=mutation):
                reference, candidate = self.evidence(True)
                row = next(row for row in candidate["rows"] if row["scenario"] == "shutdown/abortive/live/8")
                if mutation == "missing-row":
                    del row["shutdown_lifetime"]
                elif mutation in ("missing-normal", "missing-allocation"):
                    del row["pairs"][mutation.removeprefix("missing-")][0]["candidate"]["shutdown_lifetime"]
                elif mutation == "wrong-operations":
                    row["operations"] = 10000
                    for pair in row["pairs"]["normal"]:
                        pair["candidate"]["operations"] = pair["reference"]["operations"] = 10000
                elif mutation == "legacy":
                    row = next(row for row in candidate["rows"] if row["scenario"] == "shutdown/abortive")
                    row["shutdown_lifetime"] = None
                else:
                    field, bad = {"batch-leak": ("maximum_batch_end_unfinished", 1),
                                  "final-leak": ("unfinished", 1), "wrong-limit": ("live_limit", 64),
                                  "wrong-maximum": ("maximum_unfinished", 7), "wrong-count": ("dropped", 0),
                                  "wrong-batches": ("batches", 10000), "boolean": ("unfinished", False),
                                  "extra-field": ("unrecognised", 0)}[mutation]
                    row["pairs"]["normal"][0]["candidate"]["shutdown_lifetime"][field] = bad
                next(item for item in reference["rows"] if item["scenario"] == row["scenario"])["pairs"] = copy.deepcopy(row["pairs"])
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("invalid performance result schema", result.stderr)

    def test_mac_capture_rejects_forged_shutdown_lifetime_before_publication(self):
        script = (ROOT / "scripts/qualify-performance.sh").read_text()
        start = script.index("validate_rows() {")
        function = script[start:script.index("\n}\n", start) + 3]
        scenarios = (self.root / "docs/performance/scenarios.txt").read_text().splitlines()
        for mutation in (None, "row", "normal", "allocation", "operations", "legacy"):
            with self.subTest(mutation=mutation):
                _, candidate = self.evidence(False)
                row = next(row for row in candidate["rows"] if row["scenario"] == "shutdown/abortive/live/8")
                if mutation == "row":
                    row["shutdown_lifetime"]["batches"] = 10000
                elif mutation in ("normal", "allocation"):
                    row["pairs"][mutation][0]["reference"]["shutdown_lifetime"]["maximum_batch_end_unfinished"] = 1
                elif mutation == "operations":
                    row["allocation_operations"] = 500
                    for pair in row["pairs"]["allocation"]:
                        pair["candidate"]["operations"] = pair["reference"]["operations"] = 500
                elif mutation == "legacy":
                    next(item for item in candidate["rows"] if item["scenario"] == "shutdown/graceful")["shutdown_lifetime"] = None
                path = self.root / "capture-rows.jsonl"
                path.write_text("\n".join(json.dumps(item) for item in candidate["rows"]))
                result = subprocess.run(["bash", "-c", function + '\nvalidate_rows "$1"', "capture-test", str(path)],
                                        env=dict(os.environ, expected_scenarios=json.dumps(scenarios)),
                                        text=True, capture_output=True, check=False)
                self.assertEqual(result.returncode == 0, mutation is None, result.stderr)

    def test_reviewed_poll_tradeoff_does_not_accept_other_metrics(self):
        for extra_metric, reviewed in ((None, False), ("cpu_cycles", False), ("p99", False), ("cpu_cycles", True)):
            with self.subTest(extra_metric=extra_metric, reviewed=reviewed):
                reference, candidate = self.evidence(True)
                row = candidate["rows"][0]
                row["rationale_id"] = "reviewed-polls"
                candidate["accepted_regressions"] = [{
                    "id": "reviewed-polls",
                    "scenario": row["scenario"],
                    "metrics": ["task_wakes_ratio"],
                    "review": "Reviewed the additional polls against all other measured costs.",
                }]
                if reviewed:
                    candidate["accepted_regressions"][0]["metrics"].append("cpu_cycles_ratio")
                for pair in row["pairs"]["normal"]:
                    pair["candidate"]["task_wakes"] *= 1.2
                    if extra_metric == "p99":
                        pair["candidate"]["latency_ns"]["p99"] *= 1.2
                    elif extra_metric is not None:
                        pair["candidate"][extra_metric] *= 1.2
                reference["rows"][0]["pairs"] = copy.deepcopy(row["pairs"])
                result = self.compare(reference, candidate)
                self.assertEqual(result.returncode, int(extra_metric is not None and not reviewed), result.stderr)

    def test_review_requires_exact_scenario_and_metric_scope(self):
        for mutation in ("missing-metrics", "empty-metrics", "unknown-metric", "metrics-not-array",
                         "wrong-scenario", "wrong-id", "blank-review", "review-not-string"):
            with self.subTest(mutation=mutation):
                reference, candidate = self.evidence(True)
                row = candidate["rows"][0]
                row["rationale_id"] = "reviewed-polls"
                review = {
                    "id": "reviewed-polls",
                    "scenario": row["scenario"],
                    "metrics": ["task_wakes_ratio"],
                    "review": "Reviewed the additional polls against all other measured costs.",
                }
                if mutation == "missing-metrics":
                    del review["metrics"]
                elif mutation == "empty-metrics":
                    review["metrics"] = []
                elif mutation == "unknown-metric":
                    review["metrics"].append("unknown_ratio")
                elif mutation == "metrics-not-array":
                    review["metrics"] = "task_wakes_ratio"
                elif mutation == "wrong-scenario":
                    review["scenario"] = candidate["rows"][1]["scenario"]
                elif mutation == "wrong-id":
                    review["id"] = "another-review"
                elif mutation == "blank-review":
                    review["review"] = " \t\n"
                else:
                    review["review"] = 42
                candidate["accepted_regressions"] = [review]
                for pair in row["pairs"]["normal"]:
                    pair["candidate"]["task_wakes"] *= 1.2
                reference["rows"][0]["pairs"] = copy.deepcopy(row["pairs"])
                result = self.compare(reference, candidate)
                self.assertEqual(result.returncode, 1, result.stderr)

    def test_wrong_provider_missing_counter_or_changed_capture_is_rejected(self):
        for mutation in ("provider", "counter", "instructions", "capture", "mixed"):
            with self.subTest(mutation=mutation):
                reference, candidate = self.evidence(True)
                if mutation == "provider":
                    candidate["metadata"]["resource_provider"] = "unknown"
                elif mutation == "counter":
                    del candidate["rows"][0]["pairs"]["normal"][0]["candidate"]["peak_commit_bytes"]
                elif mutation == "instructions":
                    candidate["rows"][0]["instructions_retired"] = 1000
                elif mutation == "capture":
                    candidate["metadata"]["cohort"]["qualification_script_sha256"] = "0" * 64
                else:
                    reference["schema_version"] = 4
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_windows_requires_receipt_binary_and_toolchain_provenance(self):
        for field in ("source_build", "source_build_sha256", "binaries", "compiler", "cargo", "rustc_commit"):
            with self.subTest(missing=field):
                reference, candidate = self.evidence(True)
                del candidate["metadata"][field]
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

        for mismatch in ("receipt", "binary", "compiler", "receipt_hash"):
            with self.subTest(mismatch=mismatch):
                reference, candidate = self.evidence(True)
                if mismatch == "receipt":
                    candidate["metadata"]["source_build"]["candidate"]["tree"] = "e" * 40
                elif mismatch == "binary":
                    candidate["metadata"]["binaries"]["candidate_normal"]["sha256"] = "e" * 64
                elif mismatch == "compiler":
                    candidate["metadata"]["compiler"] = "rustc 1.97.0 (wrong)"
                else:
                    candidate["metadata"]["source_build_sha256"] = "e" * 64
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_consistent_old_toolchain_receipt_does_not_qualify_current_capture(self):
        reference, candidate = self.evidence(True)
        for result in (reference, candidate):
            metadata = result["metadata"]
            tools = metadata["source_build"]["toolchain"]
            tools.update(rustc="rustc 1.98.0 (pinned)", cargo="cargo 1.98.0 (pinned)")
            for target in (metadata, metadata["cohort"]):
                target.update(compiler=tools["rustc"], cargo=tools["cargo"])
            receipt = json.dumps(metadata["source_build"], sort_keys=True, indent=2).encode()
            metadata["source_build_sha256"] = digest(receipt)
        result = self.compare(reference, candidate)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("Windows capture requires pinned Rust 1.100.0", result.stderr)


if __name__ == "__main__":
    unittest.main()
