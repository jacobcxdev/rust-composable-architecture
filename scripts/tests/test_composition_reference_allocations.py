"""Exercise the cache reader, real Windows monitor receipt writer and allocation evaluators."""
import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from release_baseline import Release
import composition_runner as runner
import qualification_reference_cache as cache
import test_composition_windows_runner as windows_tests


def sha(data):
    return hashlib.sha256(data).hexdigest()


class ReferenceAllocationCaptureTests(unittest.TestCase):
    def setUp(self):
        self.release = Release("v1.0.0", "8" * 40, "a" * 40, "1" * 40, "0" * 64)
        self.enterContext(patch("release_baseline.latest_baseline", return_value=self.release))
        try:
            import composition_reference_allocations as capture
        except ModuleNotFoundError:
            self.fail("reference-only allocation capture is missing")
        self.capture = capture.capture
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.root = Path(td.name).resolve()
        self.cache = self.root / "cache"
        role = self.cache / "a"
        role.mkdir(parents=True)
        self.inputs = dict.fromkeys(cache.INPUT_HASHES, "a" * 64) | {"tree": "1" * 40}
        images = {name: sha(name.encode()) for name in ("allocation", "zero")}
        for name in images:
            (role / (name + ".exe")).write_bytes(name.encode())
        (role / "provenance.json").write_bytes(b"verified build receipt")
        entry = {"source_root": "C:/private/source", "target_root": "C:/private/target",
                 "provenance_sha256": sha(b"verified build receipt"), "products": images}
        manifest = {"schema": 1, "purpose": "composition-allocations", "revision": self.release.revision,
                    "inputs": self.inputs, "roles": {"a": entry}}
        encoded = json.dumps(manifest).encode()
        (self.cache / "enrolment.json").write_bytes(encoded)
        self.pin = sha(encoded)
        self.jobs = []
        self.damage = None
        self.policy = runner.evidence.validate_allocation_manifest(runner.evidence.DEFAULT_ALLOCATION_MANIFEST)
        self.cwd = self.root / "work"
        self.cwd.mkdir()
        self.raw = self.root / "observation"
        self.monitor = runner.WindowsMonitor(self.root / "telemetry", runner.evidence.load_protocol(runner.WINDOWS_PROTOCOL),
                                             provider=object(), run_process=self.process, complete=True, experimental_rank=True)

    def process(self, command, stream, cwd, env, *args, **kwargs):
        self.jobs.append(command)
        self.assertEqual(Path(cwd), self.cwd)
        if len(command) == 1:
            self.assertEqual(Path(command[0]), self.cache / "a/allocation.exe")
            scenario = env["RCA_COMPOSITION_ALLOCATION_SCENARIO"]
            samples = int(env["RCA_COMPOSITION_ALLOCATION_SAMPLES"]) + (self.damage == "count")
            ops = self.policy[scenario]["logical_operations"]
            stream.write((f"scenario={scenario} samples={samples} logical_operations={ops} allocations=73 "
                          "allocated_bytes=900000000 retained_bytes=0 peak_live_bytes=0\n").encode())
        else:
            self.assertEqual(Path(command[0]), self.cache / "a/zero.exe")
            self.assertEqual(command[2:], ["--exact", "--nocapture"])
            name = "wrong-test" if self.damage == "zero" else command[1]
            stream.write(f"test {name} ... ok\ntest result: ok. 1 passed; 0 failed\n".encode())
            if self.damage == "image":
                (self.cache / "a/zero.exe").write_bytes(b"different")
        fact = windows_tests.WindowsRunnerTests.process_result()
        start = 1000 if self.damage == "duplicate" else len(self.jobs) * 1000
        fact["counters"].update(CreationTime100ns=start, ExitTime100ns=start+100)
        return fact

    def collect(self):
        return self.capture(self.cache, enrolment_sha256=self.pin, inputs=self.inputs, raw=self.raw,
                            cwd=self.cwd, env={}, monitor=self.monitor)

    def test_forty_nine_fresh_jobs_report_the_actual_over_budget_reference(self):
        out = self.collect()
        self.assertEqual((len(self.jobs), len(out["allocations"]), len(out["zero_tests"]), len(out["metrics"])),
                         (49, 47, 2, 13))
        self.assertTrue(any(not m["passed"] for m in out["metrics"]))
        self.assertNotIn("decision", out)
        self.assertEqual(out, json.loads((self.raw / "result.json").read_bytes()))

    def test_wrong_count_or_wrong_zero_cannot_produce_observation(self):
        for damage in ("count", "zero", "duplicate", "image"):
            self.damage = damage
            self.raw = self.root / damage
            with self.subTest(damage=damage), self.assertRaises(ValueError):
                self.collect()
            self.assertFalse((self.raw / "result.json").exists())

    def test_bad_enrolment_or_stale_output_never_launches(self):
        self.pin = "f" * 64
        with self.assertRaises(ValueError):
            self.collect()
        self.assertFalse(self.jobs)
        self.pin = sha((self.cache / "enrolment.json").read_bytes())
        self.raw.mkdir(exist_ok=True)
        with self.assertRaises((FileExistsError, ValueError)):
            self.collect()
        self.assertFalse(self.jobs)

    def test_image_replaced_after_admission_cannot_become_the_expected_image(self):
        image = self.cache / "a/allocation.exe"

        def admission(*args, **kwargs):
            if self.jobs:  # A transient replacement is gone by closing verification.
                image.write_bytes(b"allocation")
            checked = cache.verify_reference_products(*args, **kwargs)
            if not self.jobs:
                image.write_bytes(b"substituted")
            return checked

        with patch("composition_reference_allocations.verify_reference_products", side_effect=admission), \
             self.assertRaisesRegex(ValueError, "reference image changed"):
            self.collect()
        self.assertFalse(self.jobs)
