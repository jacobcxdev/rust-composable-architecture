"""Check that stripped binaries cannot use another subject's owned build outputs."""

import json
from pathlib import Path
import shutil
import sys
import time
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_pe_seed as pe
from scripts.tests import test_composition_registry_seed as seed_fixtures
from scripts.tests import test_composition_performance as performance_fixtures


class PeSeedTests(unittest.TestCase):
    def setUp(self):
        fx = seed_fixtures.RegistrySeedTests()
        fx.setUp()
        self.addCleanup(fx.doCleanups)
        self.fx = fx
        self.support = fx.root / "capture/workspace-support"
        self.stage = fx.root / "capture/composition"
        self.support.mkdir(parents=True)
        self.stage.mkdir()
        donor = self.support / "compile-target"
        self.source = fx.lock(fx.donor).parent
        self.scratch = fx.root / "scratch"
        mode = "candidate-reference"
        self.raw = self.stage / mode / "absolute-windows-raw"
        self.target = self.scratch / mode / "absolute-windows-binary"
        self.subject = self.target.parent / "source-subject"
        self.raw.mkdir(parents=True)
        self.target.mkdir(parents=True)
        self.subject.mkdir()
        for path in ("Cargo.toml", "src/lib.rs"):
            for source in (self.source, self.subject):
                (source/path).parent.mkdir(exist_ok=True)
                (source/path).touch()
        self.locks = {"Cargo.lock": fx.lock_sha, "benches/compile/Cargo.lock": fx.lock_sha}
        for path in self.locks:
            (self.subject/path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(self.source/"Cargo.lock", self.subject/path)
        fx.donor.rename(donor)
        self.log = self.support / "clean-compile.txt"
        self.rows = [dict(fx.rows[0], filenames=[str(donor/fx.compiled)]),
                     dict(fx.rows[1], filenames=[str(donor/fx.owned)], manifest_path=str(self.source/"Cargo.toml"),
                          target={"src_path": str(self.source/"src/lib.rs")}), fx.rows[-1]]
        args = ["cargo", "build", "--locked", "--workspace", "--all-features", "--features",
                "critical-section/std", "--release", "--message-format=json"]
        self.events = [dict(phase="start", monotonic=1, command=args, destination=str(self.log), cwd=str(self.source),
                            target=str(donor), incremental="0", command_kind="compile", job_memory_limit_bytes=2*1024**3),
                       dict(phase="finish", monotonic=2, destination=str(self.log), returncode=0,
                            job_memory_limit_bytes=2*1024**3)]
        self.counters = performance_fixtures.WindowsPairedProtocolTests.resources(compiler=True)
        self.started_ns = time.time_ns() - 2_000_000_000
        start = 116444736000000000 + self.started_ns // 100 + 1
        self.counters["direct_process"].update(CreationTime100ns=start, ExitTime100ns=start+5_000_000)
        receipt = self.counters["process_receipt"]
        receipt.update(descendants_exited_naturally=False,
                       terminated_telemetry=[{"pid": 2, "image_path": receipt["compiler_telemetry_path"]}])
        self.events[1]["process_receipt"] = receipt

    def prepare(self, started_at=0):
        self.write(self.log, self.rows)
        self.log.with_suffix(".resources.json").write_text(json.dumps(self.counters), encoding="utf-8")
        self.write(self.support/"processes.jsonl", self.events)
        return pe.prepare(self.support, self.source, self.fx.home, self.stage, self.scratch,
                          started_at=started_at, started_unix_ns=self.started_ns, cargo="cargo",
                          lock_sha256=self.fx.lock_sha, transcript_sha256=pe.registry._sha(self.log))

    @staticmethod
    def write(path, rows):
        path.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")

    def test_only_registry_units_cross_roles_and_later_jobs_can_reuse_their_own_bytes(self):
        allowed = pe.install(self.prepare(), self.target, self.raw, self.locks)
        registry_out = self.target/self.fx.compiled
        self.assertIn(registry_out, allowed)
        self.assertFalse((self.target/self.fx.owned).exists())
        emitted = self.target/"example.exe"
        emitted.write_bytes(b"newly compiled")
        registry_row = dict(self.rows[0], filenames=[str(registry_out)], fresh=True)
        owned_row = dict(self.rows[1], filenames=[str(emitted)], manifest_path=str(self.subject/"Cargo.toml"),
                         executable=str(emitted), target={"src_path": str(self.subject/"src/lib.rs")})
        log = self.raw/"fixture.jsonl"
        for forged, message in (([dict(owned_row, executable=str(registry_out))], "not a reported output"),
                                ([owned_row, owned_row], "duplicate output")):
            self.write(log, [*forged, self.rows[-1]])
            with self.assertRaisesRegex(ValueError, message):
                pe.verify_build(log, self.target, self.subject, allowed)
        self.write(log, [registry_row, dict(owned_row, fresh=True), self.rows[-1]])
        with self.assertRaisesRegex(ValueError, "warm PE"):
            pe.verify_build(log, self.target, self.subject, allowed)
        self.write(log, [registry_row, owned_row, self.rows[-1]])
        pe.verify_build(log, self.target, self.subject, allowed)
        self.write(log, [registry_row, dict(owned_row, fresh=True), self.rows[-1]])
        pe.verify_build(log, self.target, self.subject, allowed)
        emitted.write_bytes(b"replaced")
        with self.assertRaisesRegex(ValueError, "warm PE"):
            pe.verify_build(log, self.target, self.subject, allowed)

    def test_donor_must_be_fresh_and_have_the_exact_verified_command(self):
        with self.assertRaisesRegex(ValueError, "wrong workspace"):
            self.prepare(started_at=1.1)
        self.events[0]["command"].remove("--all-features")
        with self.assertRaisesRegex(ValueError, "wrong workspace"):
            self.prepare()
        self.events[0]["command"].insert(4, "--all-features")
        self.rows[0]["fresh"] = True
        with self.assertRaisesRegex(ValueError, "clean compile reused"):
            self.prepare()

    def test_neither_cold_targets_nor_another_capture_can_receive_files(self):
        seed = self.prepare()
        for place in (self.target.parent/"compile-direct", self.fx.root/"other"/self.target.name):
            place.mkdir(parents=True)
            with self.assertRaisesRegex(ValueError, "empty PE target"):
                pe.install(seed, place, self.raw, self.locks)
            self.assertFalse(list(place.iterdir()))
        other = self.target.parent/"moved"
        self.scratch.rename(self.fx.root/"old-scratch")
        other.mkdir(parents=True)
        self.target.mkdir()
        with self.assertRaisesRegex(ValueError, "empty PE target"):
            pe.install(seed, self.target, self.raw, self.locks)

    def test_changed_consumer_lock_is_not_a_cache_miss(self):
        seed = self.prepare()
        (self.subject/"benches/compile/Cargo.lock").write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "lock changed"):
            pe.install(seed, self.target, self.raw, self.locks)

    def test_old_process_future_events_and_unrecognised_telemetry_are_rejected(self):
        self.started_ns += 1_000_000_000
        with self.assertRaisesRegex(ValueError, "outside the current capture"):
            self.prepare()
        self.started_ns -= 1_000_000_000
        self.events[1]["monotonic"] = float("inf")
        with self.assertRaisesRegex(ValueError, "wrong workspace"):
            self.prepare()
        self.events[1]["monotonic"] = 2
        self.counters["process_receipt"]["terminated_telemetry"][0]["image_path"] = "foreign.exe"
        with self.assertRaisesRegex(ValueError, "pinned compiler"):
            self.prepare()

    def test_only_three_binary_jobs_are_seeded_and_all_are_required(self):
        receipt = self.raw.parent/"sources/subject.json"
        receipt.parent.mkdir()
        receipt.write_text(json.dumps({"effective": self.locks}), encoding="utf-8")
        capture = {"staging": self.raw.parent, "targets": self.target.parent, "roots": {"subject": self.subject},
                   "metadata": {"workload_provenance": {"subject": {"receipt_sha256": pe.registry._sha(receipt)}}}}
        seed = self.prepare()
        caller = Mock()
        cargo = ["cargo", "build"]
        env = {"CARGO_TARGET_DIR": str(self.target)}
        seen = []

        def run(command, log, cwd, options):
            seen.append((log.name, dict(options)))
            if log.name.startswith("binary-"):
                self.assertTrue((self.target/self.fx.compiled).is_file())
                self.write(log, [dict(self.rows[0], filenames=[str(self.target/self.fx.compiled)], fresh=True),
                                 self.rows[-1]])

        caller.run.side_effect = run
        def collect(_capture, _protocol, _env, monitor, _tools):
            monitor.run(["other"], self.raw/"strip.txt", self.subject, {})
            for job in ("fixture", "tokio", "app-test"):
                monitor.run(cargo, self.raw/f"binary-{job}-build.jsonl", self.subject, env)
            return "passed"

        with patch.object(pe.absolute, "capture_windows_absolute", return_value=None):
            with self.assertRaisesRegex(ValueError, "missing"):
                pe.capture(capture, {}, {}, caller, {"cargo": "cargo"}, seed=seed)
        with patch.object(pe.absolute, "capture_windows_absolute", side_effect=collect):
            self.assertEqual(pe.capture(capture, {}, {}, caller, {"cargo": "cargo"}, seed=seed), "passed")
        self.assertEqual(seen[0], ("strip.txt", {}))
        self.assertEqual([options["CARGO_BUILD_BUILD_DIR"] for _, options in seen[1:]], [str(self.target)] * 3)
