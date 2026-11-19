"""Check the non-timing Windows command contract independently of the collector."""

import json
from pathlib import Path, PureWindowsPath
import sys
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_product_commands as product


class ProductCommandTests(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.captured = Path(self.tmp.name)
        self.mode = "candidate-reference"
        self.staged = PureWindowsPath("C:/temp/q/candidate-reference/source-subject")
        self.win_capture = PureWindowsPath("C:/capture.incomplete")
        self.raw = self.captured/self.mode/"absolute-windows-raw"
        self.raw.mkdir(parents=True)
        self.norm = self.staged.parent/"subject/release/deps/composition-normal.exe"
        self.tools = {k: {"path": "C:\\tools\\"+v} for k,v in (
            ("cargo", "cargo.exe"), ("objcopy", "llvm-objcopy.exe"), ("python", "python.exe"),
            ("lief_wheel","lief.whl"), ("lief_init", "site\\lief\\__init__.py"))}
        (self.captured/"capture.json").write_text(json.dumps({"controller": {"root": "C:\\source"}}))
        self.alloc = self.staged.parent/"subject/release/deps/composition-allocation.exe"
        self.zero = self.staged.parent/"absolute-windows-zero/debug/deps/composition_zero-123.exe"
        for alloc, path in ((True, self.alloc), (False, self.zero)):
            name, kind, src = (("composition", "bench", "rca-core/benches/composition.rs") if alloc else
                               ("composition_zero", "test", "rca-core/tests/composition_qualification/main.rs"))
            record = {"reason": "compiler-artifact", "fresh": False, "features": ["allocation-measurement"],
                      "target": {"name": name, "kind": [kind], "src_path": str(self.staged/src)}, "executable": str(path)}
            (self.raw/("allocation-build.jsonl" if alloc else "zero-build.jsonl")).write_text(json.dumps(record)+"\n")

    def commands(self):
        return product.product_commands(self.captured, self.mode, self.staged, self.tools, self.norm, self.win_capture)

    def test_every_operation_names_its_own_process_and_raw_transcript(self):
        rows = self.commands()
        self.assertEqual(len(rows), 66)
        self.assertEqual(len(set(row[0] for row in rows)), 66)
        alloc = [r for r in rows if "/allocation/" in r[0]]
        self.assertEqual(len(alloc), 47)
        self.assertTrue(all(r[1:] == ([str(self.alloc)], self.staged, "runtime", None) for r in alloc))
        find = lambda name: next(r for r in rows if r[0].endswith("/"+name))
        self.assertEqual(find("zero-direct_scope.txt")[1][0], str(self.zero))
        self.assertEqual(find("binary-tokio-build.jsonl")[4], self.staged.parent/"absolute-windows-binary")
        labels = [Path(row[0]).name for row in rows]
        self.assertLess(labels.index("debug-strip-tokio-application.txt"), labels.index("binary-app-test-build.jsonl"))
        raw = self.win_capture/self.mode/"absolute-windows-raw"
        self.assertEqual(find("strip-tokio-application.txt")[1:4], (
            [self.tools["objcopy"]["path"], "--strip-all", str(raw/"binaries/tokio-application-original.exe"),
             str(raw/"binaries/tokio-application-llvm-stripped.exe")], raw, "runtime"))

    def test_foreign_warm_or_nonallocation_executable_cannot_supply_counts(self):
        log = self.raw/"allocation-build.jsonl"
        original = json.loads(log.read_text())
        for update in ({"fresh": True}, {"features": []}, {"features": "not-allocation-measurement"},
                       {"target": {**original["target"], "kind": "not-a-bench"}},
                       {"executable": str(self.norm)},
                       {"executable": "C:\\elsewhere\\composition.exe"},
                       {"executable": str(self.staged.parent/"subject/../control/m.exe")},
                       {"target": {**original["target"], "src_path": str(self.staged.parent/"other.rs")}}):
            log.write_text(json.dumps({**original, **update}))
            with self.subTest(update=update), self.assertRaises(ValueError):
                self.commands()
        log.write_text((json.dumps(original)+"\n")*2)
        with self.assertRaises(ValueError):
            self.commands()

    def test_parent_segments_cannot_cross_role_or_capture_directories(self):
        for staged, capture in ((self.staged.parent/"../candidate-reference/source-subject", self.win_capture),
                                (self.staged, self.win_capture/"../another-capture")):
            with self.subTest(staged=staged, capture=capture), self.assertRaises(ValueError):
                product.product_commands(self.captured, self.mode, staged, self.tools, self.norm, capture)
