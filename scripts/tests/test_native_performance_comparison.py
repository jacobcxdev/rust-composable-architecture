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
            "scripts/qualify-performance.sh": b"Mac capture\n",
            "scripts/guard_cargo_memory.py": b"Mac memory watchdog\n",
            "scripts/capture-windows-performance.py": b"Windows capture\n",
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
        scenario_hash = digest((self.root / "docs/performance/scenarios.txt").read_bytes())
        capture_hash = digest((self.root / capture).read_bytes())
        cohort = {
            "id": "c" * 64,
            "recorded_at": "2026-10-30T23:00:00Z",
            "runner": "test-windows" if windows else "test-mac",
            "protocol_sha256": digest(f"{harness_hash}\n{scenario_hash}\n{capture_hash}\n".encode()),
            "qualification_source_sha256": harness_hash,
            "qualification_script_sha256": capture_hash,
            "scenario_manifest_sha256": scenario_hash,
            "candidate_lock_sha256": digest(b"candidate lock\n"),
            "reference_lock_sha256": digest(b"reference lock\n"),
        }
        if not windows:
            cohort["memory_watchdog_sha256"] = digest((self.root / "scripts/guard_cargo_memory.py").read_bytes())
            cohort["memory_watchdog_receipt"] = str((self.root / "memory.json").resolve())
            cohort["protocol_sha256"] = digest(
                f"{harness_hash}\n{scenario_hash}\n{capture_hash}\n{cohort['memory_watchdog_sha256']}\n".encode())
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
            tools = {"rustc": "rustc 1.99.0 (pinned)",
                     "cargo": "cargo 1.99.0 (pinned)",
                     "rustc_commit": "6" * 40,
                     "target": "x86_64-pc-windows-msvc"}
            for target in (metadata, cohort):
                target.update(compiler=tools["rustc"], cargo=tools["cargo"], rustc_commit=tools["rustc_commit"])
            source = {"harness_sha256": harness_hash, "scenario_manifest_sha256": scenario_hash,
                      "toolchain": tools, "binaries": {}}
            metadata["binaries"] = {}
            for role, revision in (("candidate", CANDIDATE), ("reference", REFERENCE)):
                source[role] = {"revision": revision, "tree": cohort[f"{role}_source_tree"],
                                "lock_sha256": cohort[f"{role}_lock_sha256"],
                                "manifest_sha256": cohort[f"{role}_manifest_sha256"],
                                "original_harness_sha256": harness_hash}
                for mode in ("normal", "allocation"):
                    key = f"{role}_{mode}"
                    source["binaries"][key] = {"sha256": digest(key.encode())}
                    metadata["binaries"][key] = {"path": f"C:/capture/{key}.exe", "sha256": digest(key.encode())}
            metadata["source_build"] = source
            metadata["source_build_sha256"] = digest(json.dumps(source, sort_keys=True, indent=2).encode())
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
                normal.update(peak_working_set_bytes=1000, peak_commit_bytes=1000)
            else:
                normal.update(peak_resident_bytes=1000, peak_memory_footprint_bytes=1000)
            allocation = dict(normal, samples=allocation_samples, operations=allocation_samples,
                              allocations=1000, allocated_bytes=1000, peak_bytes=1000)
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
                             allocation_samples=allocation_samples, allocation_operations=allocation_samples,
                             normal_repetitions=10, allocation_repetitions=6,
                             pairs=pairs, rationale_id=None))
        candidate = dict(schema_version=5 if windows else 4, suite="rca-performance-qualification",
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
                entrypoint="qualify-performance", toolchain="1.99.0", started=True,
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
                            cohort["scenario_manifest_sha256"] + "\n" +
                            cohort["qualification_script_sha256"] + "\n").encode())
                result = self.compare(reference, candidate)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_mac_rejects_watchdog_source_changed_after_capture(self):
        reference, candidate = self.evidence(False)
        (self.root / "scripts/guard_cargo_memory.py").write_text("changed watchdog\n")
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
        self.assertIn("Windows source/build receipt does not match measured identities", result.stderr)


if __name__ == "__main__":
    unittest.main()
