"""A verified B image can set only the six size limits; C must still build."""

import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from release_baseline import Release
import composition_reference_sizes as sizes
from scripts.tests import test_composition_windows_absolute as images


class ReferenceSizesTests(unittest.TestCase):
    def setUp(self):
        self.release = Release("v1.0.0", "8" * 40, "a" * 40, "a" * 40, "0" * 64)
        self.enterContext(patch("release_baseline.latest_baseline", return_value=self.release))
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.cache = self.root / "cache"

    def test_one_byte_over_existing_rounded_limit_fails_for_each_product(self):
        reference = {name: 1001 for name in sizes.NAMES}
        bins = {name: {"bytes": 1102} for name in sizes.NAMES}
        self.assertTrue(sizes._compare(reference, bins)["passed"])
        for name in sizes.NAMES:
            bad = {**bins, name: {"bytes": 1103}}
            result = sizes._compare(reference, bad)
            self.assertEqual([r["scenario"] for r in result["checks"] if not r["passed"]], [f"binary/{name}"])
        with self.assertRaisesRegex(ValueError, "all six"):
            sizes._compare(reference, {})

    def test_wrong_archive_pin_or_existing_cache_fails_before_enrolment(self):
        archive = self.root/"raw.tar.gz"
        archive.write_bytes(b"not enrolled")
        with self.assertRaisesRegex(ValueError, "independent enrolment pin"):
            sizes.enrol(self.cache, archive=archive, archive_sha256="0"*64)
        self.assertFalse(self.cache.exists())
        self.cache.mkdir()
        flag = self.cache / "mine"
        flag.write_text("unchanged")
        with self.assertRaisesRegex(ValueError, "new external cache"):
            sizes.enrol(self.cache, archive=archive, archive_sha256="0"*64)
        self.assertEqual(flag.read_text(), "unchanged")

    def test_enrols_b_only_and_rejects_swapped_images(self):
        archive = self.root/"unused"
        inputs = dict(tree="a"*40, **{name:"b"*64 for name in (
            "effective_source_sha256","lock_sha256","workload_sha256","toolchain_sha256","configuration_sha256","platform_sha256")})
        def prove(scratch):
            raw = scratch/sizes.MODE/"absolute-windows-raw"
            (raw/"binaries").mkdir(parents=True)
            rows = {}
            for i, name in enumerate(sizes.NAMES):
                rel = f"binaries/{name}-stripped.exe"
                image = images.cleared(images.image()) + bytes(i)
                (raw/rel).write_bytes(image)
                rows[name] = {"path": rel, "bytes": len(image), "sha256": hashlib.sha256(image).hexdigest()}
            return inputs, {"revision": self.release.revision}, "C:/ref/source", "C:/ref/target", rows
        with patch.object(sizes, "_extract_reference"), patch.object(sizes, "_prove", side_effect=prove):
            pin, _ = sizes.enrol(self.cache, archive=archive, archive_sha256="b"*64)
        self.assertEqual(set(sizes.reference_sizes(self.cache, enrolment_sha256=pin, inputs=inputs)), set(sizes.NAMES))
        self.assertEqual(set(json.loads((self.cache/"enrolment.json").read_bytes())["roles"]), {"b"})
        (self.cache/"b/direct.exe").write_bytes((self.cache/"b/scoped.exe").read_bytes())
        with self.assertRaisesRegex(ValueError, "executable changed"):
            sizes.reference_sizes(self.cache, enrolment_sha256=pin, inputs=inputs)

    def test_only_candidate_role_can_dispatch(self):
        capture = {"mode": sizes.MODE, "staging": self.root, "targets": self.root, "roots":{"subject":self.root},
                   "metadata": {}}
        with patch.object(sizes.absolute, "_run_binary_builds") as build, self.assertRaisesRegex(ValueError, "fresh candidate"):
            sizes.compare_fresh(capture, {}, Mock(), {}, cache=self.cache, enrolment_sha256="0"*64,
                                candidate_revision="1"*40, candidate_tree="2"*40)
        build.assert_not_called()

    def test_current_sources_and_tools_drive_fresh_candidate_and_limits(self):
        mode = "candidate-reference"
        stage, targets = self.root/"evidence"/mode, self.root/"targets"/mode
        (stage/"sources").mkdir(parents=True)
        tools, facts = {}, {}
        for name in ("rustc","rustdoc","cargo","objcopy","python","lief_wheel","lief_init","lief_native"):
            file = self.root/name
            file.write_bytes(name.encode())
            tools[name] = str(file)
            facts[name] = {"path":str(file),"sha256":sizes.evidence._sha256(file)}
        meta = {"mode":mode,"method_version":sizes.evidence.WINDOWS_V6_METHOD,"subject_revision":"1"*40,
                "control_revision":self.release.revision,"subject_role":"candidate","control_role":"reference",
                "measured_subject_revision":"1"*40,"toolchain":"test toolchain","workload_sha256":"c"*64,
                "protocol_sha256":sizes.evidence._sha256(sizes.runner.WINDOWS_PROTOCOL),
                "budget_sha256":sizes.evidence._sha256(sizes.runner.BUDGET),"workload_provenance":{},
                "platform":"windows","target":"x86_64-pc-windows-msvc","os_version":"11","os_build":"1","runner":"host"}
        active = {"mode":mode,"tools":facts,"measured_subject":{"revision":"1"*40,"tree":"2"*40}}
        capture = {"mode":mode,"metadata":meta,"staging":stage,"targets":targets,"roots":{}}
        for role in ("control","subject"):
            src = targets/("source-"+role)
            for file in ("Cargo.toml","Cargo.lock","benches/compile/Cargo.toml"):
                path = src/file
                path.parent.mkdir(parents=True,exist_ok=True)
                path.write_text(file)
            original = sizes.runner.source_hashes(src)
            tree = ("2" if role=="subject" else "3")*40
            fact = {"tree":tree,"revision":meta[role+"_revision"],"hashes":original}
            receipt = stage/"sources"/(role+".json")
            receipt.write_text(json.dumps({**fact,"root":str(src),"effective":original,"original":original}))
            meta["workload_provenance"][role] = {"receipt_sha256":sizes.evidence._sha256(receipt)}
            meta[role+"_lock_sha256"] = original["Cargo.lock"]
            capture["roots"][role] = src
            active[role] = capture[role] = fact
        def save():
            (stage/"inputs.json").write_text(json.dumps(active))
        def run():
            return sizes.compare_fresh(capture, {}, Mock(), tools, cache=self.cache, enrolment_sha256="f"*64,
                                       candidate_revision="1"*40,candidate_tree="2"*40)
        save()
        with patch.object(sizes, "reference_sizes", return_value={k:100 for k in sizes.NAMES}) as verify, \
             patch.object(sizes.absolute, "_run_binary_builds", return_value={k:{"bytes":111} for k in sizes.NAMES}) as build:
            self.assertFalse(run()["passed"])
            build.assert_called_once()
            self.assertEqual(build.call_args.args[3]["CARGO_BUILD_BUILD_DIR"], str(targets/"absolute-windows-binary"))
            self.assertEqual(verify.call_args.kwargs["inputs"]["tree"], "3"*40)
            active["control"] = {**active["control"],"tree":"4"*40}
            save()
            with self.assertRaisesRegex(ValueError, "control source"):
                run()
            active["measured_subject"]["revision"] = "a"*40
            save()
            with self.assertRaisesRegex(ValueError, "enrolled candidate"):
                run()
            build.assert_called_once()
