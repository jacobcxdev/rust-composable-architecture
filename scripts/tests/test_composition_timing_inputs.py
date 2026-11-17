"""Check the input boundary for independently measured adaptive Windows timing."""

import hashlib
import json
from pathlib import Path, PureWindowsPath
import sys
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_performance as evidence
import composition_timing_inputs as timing
from scripts.tests import test_composition_performance as fixtures


class TimingInputTests(unittest.TestCase):
    def setUp(self):
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.protocol = evidence.load_protocol(
            Path(__file__).resolve().parents[2] / "docs/performance/composition/composition-windows-paired-v6.json")
        self.counts = {f"compile/{s}": 1 for s in self.protocol["compile"]["fixtures"]}
        self.counts.update({f"runtime/{s}": 1 for s in self.protocol["runtime"]["gated_scenarios"]})

    def test_retiring_a_cell_keeps_its_pairs_in_both_independent_modes(self):
        self.counts["compile/direct"] = 2
        jobs = timing._jobs(self.protocol, self.counts)
        self.assertEqual(len(jobs), 56)
        self.assertEqual([(m, r, w) for n, m, r, w, _ in jobs if n == 2], [
            ("candidate-reference", "subject", "compile/direct"),
            ("candidate-reference", "control", "compile/direct"),
            ("reference-reference-aa", "subject", "compile/direct"),
            ("reference-reference-aa", "control", "compile/direct")])
        for incorrect in (dict(self.counts, **{"compile/direct": 0}),
                          dict(self.counts, **{"compile/direct": 21}),
                          dict(self.counts, **{"compile/direct": True}),
                          {k:v for k,v in self.counts.items() if k != "compile/scoped"}):
            with self.subTest(incorrect=incorrect), self.assertRaises(ValueError):
                timing._jobs(self.protocol, incorrect)

    def test_changed_missing_and_extra_observations_all_fail_closed(self):
        jobs = timing._jobs(self.protocol, self.counts)
        seals = {m: {} for m in ("candidate-reference", "reference-reference-aa")}
        for _, mode, _, _, rel in jobs:
            path = self.dir / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            for f in (path, path.with_suffix(".resources.json")):
                f.write_bytes(b"original")
                seals[mode][f.relative_to(self.dir).as_posix()] = hashlib.sha256(b"original").hexdigest()
        timing._verify_seals(self.dir, jobs, seals)
        extra = path.with_name("block-20.txt")
        extra.write_bytes(b"extra")
        with self.assertRaises(ValueError):
            timing._verify_seals(self.dir, jobs, seals)
        extra.unlink()
        path.write_bytes(b"changed")
        with self.assertRaises(ValueError):
            timing._verify_seals(self.dir, jobs, seals)
        path.unlink()
        with self.assertRaises(ValueError):
            timing._verify_seals(self.dir, jobs, seals)

    def test_complete_looking_capture_without_matching_protocol_is_rejected(self):
        (self.dir/"capture.json").write_text(json.dumps({"protocol_sha256": "a"*64}))
        with self.assertRaises(ValueError):
            timing._identities(self.dir, self.protocol)

    def test_changed_collector_or_cargo_digest_cannot_borrow_a_capture_identity(self):
        root = PureWindowsPath("C:/rca")
        cargo = {"path": "C:\\rust\\cargo.exe", "sha256": "f"*64}
        inputs = {"controller":{"root":str(root), "hashes":{"scripts/capture-composition-performance.sh":"a"*64}},
                  "tools":{"cargo":cargo}, "input_hashes":{cargo["path"]:cargo["sha256"]}}
        meta = {"capture_script_sha256":"a"*64}
        for field, name in (("qualification_script_sha256", "composition_runner.py"),
                            ("evaluator_script_sha256", "composition_performance.py")):
            meta[field] = "a"*64
            inputs["input_hashes"][str(root / "scripts" / name)] = meta[field]
        timing._verify_tools(inputs, meta)
        with self.assertRaises(ValueError):
            timing._verify_tools(inputs, {**meta, "qualification_script_sha256":"e"*64})
        cargo["sha256"] = "9"*64
        with self.assertRaises(ValueError):
            timing._verify_tools(inputs, meta)

    def test_missing_empty_or_modified_arm_protocol_cannot_inherit_current_policy(self):
        schedule = [{"block":n, "mode":m} for n in range(1, 21)
                    for m in evidence.interleaved_mode_order(self.protocol, n)]
        (self.dir / "capture.json").write_text(json.dumps({
            "protocol_sha256":evidence.WINDOWS_V6_PROTOCOL_SHA256, "schedule":schedule,
            "modes":{m:{} for m in timing.MODES}}))
        arm = self.dir / "reference-reference-aa"
        arm.mkdir()
        for contents in (None, "", json.dumps({**self.protocol, "regression_limit": 1.5})):
            if contents is not None:
                (arm / "protocol.json").write_text(contents)
            with self.subTest(contents=contents), self.assertRaises(ValueError):
                timing._identities(self.dir, self.protocol)

    def test_runtime_launch_uses_only_its_declared_window(self):
        job = next(j for j in timing._jobs(self.protocol, self.counts) if j[3] == "runtime/direct/1")
        _, mode, role, _, rel = job
        source = PureWindowsPath("C:/scratch") / mode / f"source-{role}"
        image = source.parent / role / "composition.exe"
        identities = {(mode, role): (source, PureWindowsPath("C:/rust/cargo.exe"), image)}
        common = {"destination": str(PureWindowsPath("C:/capture") / rel),
                  "job_memory_limit_bytes": self.protocol["process_memory_limit_bytes"]}
        start = {**common, "phase": "start", "monotonic": 1, "command_kind": "runtime",
                 "timeout_seconds": self.protocol["process_timeout_seconds"], "cwd": str(source)}
        receipt = {"divan_arguments": ["--min-time", "0.5", "--max-time", "0.5"]}
        finish = {**common, "phase": "finish", "monotonic": 2, "returncode": 0, "process_receipt": receipt}
        resource = (self.dir / rel).with_suffix(".resources.json")
        resource.parent.mkdir(parents=True)
        resource.write_text(json.dumps({"process_receipt": receipt, "direct_process": {
            "CreationTime100ns": 1, "ExitTime100ns": 2}}))
        trace = self.dir / "telemetry/processes.jsonl"
        trace.parent.mkdir()

        for window, experimental, accepted in (("0.5", True, True), ("2", True, False),
                                               ("0.25", True, False), ("0.5", False, False)):
            command = [str(image), "--bench", "--exact", "composition::direct::1",
                       "--min-time", window, "--max-time", window]
            trace.write_text(json.dumps({**start, "command": command}) + "\n" + json.dumps(finish))
            with self.subTest(window=window, experimental=experimental):
                if accepted:
                    timing._verify_processes(self.dir, [job], identities, self.protocol, experimental_rank=experimental)
                else:
                    with self.assertRaisesRegex(ValueError, "runtime image or command"):
                        timing._verify_processes(self.dir, [job], identities, self.protocol, experimental_rank=experimental)

    def test_each_cold_process_must_use_its_own_empty_target_and_elapsed_receipt(self):
        jobs = [job for job in timing._jobs(self.protocol, self.counts) if job[3] == "compile/direct"]
        events, roles = [], {}
        trace = self.dir / "telemetry/processes.jsonl"
        trace.parent.mkdir()
        for i, (_, mode, role, _, rel) in enumerate(jobs):
            source = PureWindowsPath("C:/scratch") / mode / f"source-{role}"
            cargo = PureWindowsPath("C:/rust/cargo.exe")
            roles[mode, role] = source, cargo, source.parent/role/"composition.exe"
            target = source.parent/f"compile-direct-01-{role}"
            common = {"destination":str(PureWindowsPath("C:/capture")/rel),
                      "job_memory_limit_bytes": 2*1024**3}
            receipt = {"leader_elapsed_seconds": 12.0+i}
            resources = {"process_receipt": receipt, "direct_process": {"CreationTime100ns": i*20,
                                                                         "ExitTime100ns": i*20+15}}
            blob = (self.dir/rel).with_suffix(".resources.json")
            blob.parent.mkdir(parents=True)
            blob.write_text(json.dumps(resources))
            events.extend([{**common, "phase":"start", "monotonic":i*20, "cwd":str(source),
                            "target":str(target), "incremental":"0", "timeout_seconds":1800,
                            "command_kind":"compile", "compiler_telemetry":self.protocol["compiler_telemetry"],
                            "command":[str(cargo), "check", "--manifest-path",
                                       str(source/"benches/compile/Cargo.toml"), "-p", "rca-compile-direct",
                                       "--locked", "-j", "1"]},
                           {**common, "phase":"finish", "monotonic":i*20+15, "returncode":0,
                            "process_receipt":receipt}])

        def verify(items):
            trace.write_text("\n".join(json.dumps(x) for x in items))
            timing._verify_processes(self.dir, jobs, roles, self.protocol)

        verify(events)
        for name, at, key, wrong in [
            ("borrowed target", 2, "target", events[0]["target"]),
            ("warm", 0, "incremental", "1"),
            ("wrong role", 2, "cwd", events[0]["cwd"]),
            ("changed elapsed", 1, "process_receipt", {"leader_elapsed_seconds": 1.0}),
            ("other staged manifest", 2, "command", events[0]["command"]),
            ("missing launch", 2, "phase", "finish"),
        ]:
            bad = events.copy()
            bad[at] = {**bad[at], key:wrong}
            with self.subTest(name=name), self.assertRaises(ValueError):
                verify(bad)
        with self.assertRaises(ValueError):
            verify(events[:-2])
        resources["direct_process"] = {"CreationTime100ns": 0, "ExitTime100ns": 15}
        blob.write_text(json.dumps(resources))
        with self.assertRaisesRegex(ValueError, "reused"):
            verify(events)

    def test_terminal_allocation_requires_its_exact_dispatch_and_finished_receipt(self):
        stage, cwd = PureWindowsPath("C:/capture"), PureWindowsPath("C:/scratch/source-subject")
        rel = "candidate-reference/absolute-windows-raw/allocation/case_1.txt"
        cmd = [r"C:\scratch\bench.exe"]
        spec = (rel, cmd, cwd, "runtime", None)
        path = self.dir / rel
        path.parent.mkdir(parents=True)
        path.write_text("case_1\n")
        resources = fixtures.WindowsPairedProtocolTests.resources()
        path.with_suffix(".resources.json").write_text(json.dumps(resources))
        dst = str(stage/rel)
        common = {"destination":dst, "job_memory_limit_bytes":2*1024**3}
        start = {**common, "phase":"start", "command":cmd, "cwd":str(cwd), "command_kind":"runtime",
                 "compiler_telemetry":None, "incremental":None, "target":None, "timeout_seconds":1800,
                 "allocation_scenario":"case_1", "allocation_samples":"7"}
        finish = {**common, "phase":"finish", "returncode":0, "process_receipt":resources["process_receipt"]}

        def verify(a, b, root=stage):
            timing._terminal_pair(self.dir, self.protocol, root, spec, a, b, {"case_1":{"samples":7}})

        verify(start, finish)
        for key, bad in (("allocation_scenario", "another"), ("allocation_samples", 7), ("cwd", "C:/other"),
                         ("target", "C:/cache"), ("command", ["different.exe"])):
            with self.subTest(key=key), self.assertRaises(ValueError):
                verify({**start, key:bad}, finish)
        with self.assertRaises(ValueError):
            verify(start, {**finish,"process_receipt":{}})
        with self.assertRaises(ValueError):
            verify(start, finish, PureWindowsPath("C:/other"))
        for field in ("incremental", "target", "compiler_telemetry"):
            with self.subTest(missing=field), self.assertRaises(ValueError):
                verify({k:v for k,v in start.items() if k != field}, finish)

    def test_final_host_and_both_admissions_are_required(self):
        host = {"identity":{"id":"host-a"}}
        stamp = "2026-11-17T21:00:00Z"
        rows = [{"recorded_at":stamp, "monotonic":i, "phase":phase, "admission":"recorded-background-load"}
                for i, phase in enumerate(("initial", "block-01", "block-01"), 1)]
        rows.append({"recorded_at":stamp, "monotonic":10, "phase":"final-host", "host":host})
        (self.dir/"telemetry").mkdir()
        def verify(events):
            (self.dir/"telemetry/telemetry.jsonl").write_text("\n".join(map(json.dumps, events)))
            timing._host_events(self.dir, 1, host)
        verify(rows)
        for bad in (rows[:-1], rows[1:], rows[:2]+rows[3:], rows[:-1]+[{**rows[-1],"host":{}}]):
            with self.subTest(events=bad), self.assertRaises(ValueError):
                verify(bad)

    def test_terminal_diagnostic_link_is_not_evidence(self):
        target = self.dir/"unrelated.txt"
        target.write_text("not a diagnostic")
        area = self.dir/"candidate-reference/diagnostics"
        area.mkdir(parents=True)
        (area/"candidate.txt").symlink_to(target)
        with self.assertRaises(ValueError):
            timing._reject_redirects(self.dir)

    def test_exact_successful_objcopy_can_leave_its_transcript_empty(self):
        stage = PureWindowsPath("C:/capture")
        rel = "candidate-reference/absolute-windows-raw/strip-direct.txt"
        cwd = (stage/rel).parent
        cmd = ["C:\\tools\\llvm-objcopy.exe", "--strip-all", "original.exe", "intermediate.exe"]
        path = self.dir/rel
        path.parent.mkdir(parents=True)
        path.touch()
        resources = fixtures.WindowsPairedProtocolTests.resources()
        path.with_suffix(".resources.json").write_text(json.dumps(resources))
        common = {"destination":str(stage/rel), "job_memory_limit_bytes":2*1024**3}
        start = {**common, "phase":"start", "command":cmd, "cwd":str(cwd), "command_kind":"runtime",
                 "incremental":None, "target":None, "compiler_telemetry":None, "timeout_seconds":1800}
        finish = {**common, "phase":"finish", "returncode":0, "process_receipt":resources["process_receipt"]}
        timing._terminal_pair(self.dir, self.protocol, stage, (rel,cmd,cwd,"runtime",None), start, finish, {})
        with self.assertRaises(ValueError):
            timing._terminal_pair(self.dir, self.protocol, stage, (rel,cmd,cwd,"runtime",None),
                                  start, {**finish,"returncode":1}, {})
