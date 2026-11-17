"""Cross-capture identity checks do not interpret any performance outcome."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


class WindowsPerformanceInputs(unittest.TestCase):
    def setUp(self):
        try:
            import check_windows_performance_inputs as join
        except ModuleNotFoundError:
            self.fail("Windows performance input checker is missing")
        self.join = join
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.native, self.comp, self.support = self.root / "native", self.root / "comp", self.root / "support.json"
        self.arm = self.comp / "candidate-reference"
        self.arm.mkdir(parents=True)
        self.native.mkdir()
        self.revs = ["c" * 40, "a" * 40, self.join.RELEASE_BASELINE_REVISION]
        self.sources = [dict(revision=r, tree=t * 40, hashes={"Cargo.lock": t * 64})
                        for r, t in zip(self.revs, "123")]
        self.host = dict(system="Windows", hostname="JACOB-PC-H1", os="Windows-11-10.0.26200-SP0",
                         machine_model="AMD64", cpu="AMD Ryzen 9 3900X 12-Core Processor", logical_cpus=24)
        self.comp_host = dict(self.host, os_version="10.0.26200", os_build="26200",
                              platform="windows", target="x86_64-pc-windows-msvc")
        self.tools = {n: {"sha256": d * 64} for n, d in zip(("rustc", "cargo", "rustdoc"), "def")}
        self.tc = dict(join.RUST100_TOOL_VERSIONS, target=self.comp_host["target"],
                       rustc_commit="0ef1bc386059596dada8fb65b50425aab2ea637a")
        del self.tc["rustdoc"]
        roles = [dict(revision=s["revision"], tree=s["tree"], lock_sha256=s["hashes"]["Cargo.lock"])
                 for s in self.sources[1:]]
        self.build = dict(candidate=roles[0], reference=roles[1], toolchain=self.tc)
        self.meta = dict(self.host, cohort=dict(self.host, qualification_script_sha256="7" * 64))
        self.capture = dict(experiment={"method": "finite-rank-v3"}, host={"identity": self.comp_host},
                            controller=self.sources[0], measured_subject=self.sources[1], control=self.sources[2])
        self.support_doc = dict(method="windows-runtime-support-v1", host={"identity": self.comp_host.copy()},
                                candidate=roles[0].copy(), tools=self.tools, toolchain=self.tc)
        self.git = {s["revision"]: (s["tree"], s["hashes"]["Cargo.lock"]) for s in self.sources}

    def write(self):
        def put(path, obj):
            data = json.dumps(obj).encode()
            path.write_bytes(data)
            return hashlib.sha256(data).hexdigest()
        build = put(self.native / "source-build.json", self.build)
        inputs = put(self.native / "inputs.json", {"metadata": self.meta})
        put(self.native / "experimental-rank-result.json", {"inputs_sha256": inputs, "source_build_sha256": build,
                                                           "numerical_decision": "fail"})
        arm = put(self.arm / "inputs.json", {"tools": self.tools})
        self.capture["modes"] = {"candidate-reference": {"inputs.json": arm}}
        capture = put(self.comp / "capture.json", self.capture)
        put(self.comp / "experimental-rank-result.json", {"capture_sha256": capture, "decision": "fail"})
        self.support_doc.setdefault("source_build", {"sha256": build})
        put(self.support, self.support_doc)

    def check(self, controller=None):
        self.write()
        with patch.object(self.join, "signed_source", side_effect=self.git.__getitem__), \
             patch.object(self.join, "git_blob_hash", return_value="7" * 64):
            return self.join.check_inputs(self.native, self.comp, self.support, controller or self.revs[0],
                                          self.revs[1], self.revs[2])

    def test_agreement_is_not_a_numerical_verdict(self):
        self.assertIsNone(self.check())  # Both fixture outcomes are failures.

    def test_another_release_reference_is_not_admissible(self):
        with self.assertRaisesRegex(ValueError, "frozen release baseline"):
            self.join.check_inputs(self.native, self.comp, self.support, *self.revs[:2], "d" * 40)

    def test_swapped_native_product_and_reference(self):
        self.build["candidate"], self.build["reference"] = self.build["reference"], self.build["candidate"]
        with self.assertRaisesRegex(ValueError, "candidate"):
            self.check()

    def test_matching_capture_claims_cannot_substitute_another_signed_controller(self):
        with self.assertRaisesRegex(ValueError, "controller"):
            self.check(controller=self.revs[1])

    def test_agreeing_claims_cannot_substitute_another_original_lock(self):
        self.sources[1]["hashes"]["Cargo.lock"] = self.build["candidate"]["lock_sha256"] = "6" * 64
        with self.assertRaisesRegex(ValueError, "signed Git"):
            self.check()

    def test_no_digest_is_not_a_seal(self):
        self.write()
        result = self.native / "experimental-rank-result.json"
        result.write_text(json.dumps(dict(json.loads(result.read_bytes()), inputs_sha256=None)))
        with patch.object(self.join, "signed_source", side_effect=self.git.__getitem__), \
             patch.object(self.join, "git_blob_hash", return_value="7" * 64), \
             self.assertRaisesRegex(ValueError, "changed input"):
            self.join.check_inputs(self.native, self.comp, self.support, *self.revs)

    def test_native_host_and_support_os_must_match(self):
        self.meta["cohort"]["hostname"] = "OTHER-PC"
        with self.assertRaisesRegex(ValueError, "host"):
            self.check()
        self.meta["cohort"].update(self.host)
        self.support_doc["host"]["identity"]["os_build"] = "99999"
        with self.assertRaisesRegex(ValueError, "host"):
            self.check()

    def test_support_from_another_native_build(self):
        self.support_doc["source_build"] = {"sha256": "8" * 64}
        with self.assertRaisesRegex(ValueError, "source-build"):
            self.check()

    def test_matching_short_versions_do_not_allow_other_compiler_bytes(self):
        self.support_doc["tools"] = dict(self.tools, rustc={"sha256": "9" * 64})
        with self.assertRaisesRegex(ValueError, "tool"):
            self.check()

    def test_git_object_without_a_signed_commit_is_rejected(self):
        blob = subprocess.check_output(["git", "rev-parse", "HEAD:Cargo.lock"], cwd=self.join.ROOT, text=True).strip()
        with self.assertRaisesRegex(ValueError, "signed"):
            self.join.signed_source(blob)
