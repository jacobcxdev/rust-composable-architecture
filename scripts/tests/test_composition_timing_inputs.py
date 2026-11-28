"""Check the input boundary for independently measured adaptive Windows timing."""

import hashlib
import json
from pathlib import Path, PureWindowsPath
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_performance as evidence
import composition_timing_inputs as timing
from scripts.tests import test_composition_performance as fixtures


class TimingInputTests(unittest.TestCase):
    def setUp(self):
        self.reference = {"mode": "release-comparison", "revision": "a" * 40,
                          "tree": "a" * 40, "lock_sha256": "b" * 64}
        self.enterContext(patch("native_release_reference.composition_reference", return_value=self.reference))
        self.tmp = TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.protocol = evidence.load_protocol(
            Path(__file__).resolve().parents[2] / "benches/performance/composition-windows.json")
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

    def test_second_look_balances_the_first_seven_pairs_and_preserves_the_other_slots(self):
        frozen = evidence.seeded_order_schedule(self.protocol)
        schedule = timing.timing_schedule(self.protocol, two_look=True)
        self.assertEqual(timing.timing_schedule(self.protocol), frozen)
        self.assertEqual(schedule[:20], frozen[:13] + [frozen[14], frozen[13]] + frozen[15:])
        self.assertEqual(schedule[:14].count("control-subject"), 7)
        self.assertEqual(schedule[20:], ["control-subject", "subject-control",
                                        "subject-control", "control-subject"])
        self.counts["compile/direct"] = 24
        jobs = timing._jobs(self.protocol, self.counts, two_look=True)
        for block in range(21, 25):
            extra = [(mode, role, work) for n, mode, role, work, _ in jobs if n == block]
            modes = evidence.interleaved_mode_order(self.protocol, block - 4)
            self.assertEqual(extra, [(mode, role, "compile/direct") for mode in modes
                                     for role in schedule[block - 1].split("-")])
        with self.assertRaises(ValueError):
            timing._jobs(self.protocol, self.counts)
        self.counts["compile/direct"] = 25
        with self.assertRaises(ValueError):
            timing._jobs(self.protocol, self.counts, two_look=True)

    def test_second_look_cannot_read_a_twenty_block_capture_as_its_own(self):
        (self.dir / "capture.json").write_text(json.dumps({}))
        with self.assertRaisesRegex(ValueError, "two-look"):
            timing._identities(self.dir, self.protocol, two_look=True)
        (self.dir / "capture.json").write_text(json.dumps({"experiment": {"method": "two-look-seven-twelve-v1"}}))
        with self.assertRaisesRegex(ValueError, "two-look"):
            timing._identities(self.dir, self.protocol, two_look=True)

    def test_enrolled_arm_plans_include_all_24_physical_orders(self):
        schedule = timing.timing_schedule(self.protocol, two_look=True)
        for mode in timing.MODES:
            plan = evidence.measurement_plan(self.protocol, mode, two_look=True)
            for kind in ("runtime", "compile"):
                physical = {r["block"]: r["stratum"] for r in plan[kind]}
                self.assertEqual(physical, dict(enumerate(schedule, 1)))
            path = self.dir / "plan.json"
            path.write_text(json.dumps(plan))
            evidence._validate_measurement_plan(path, self.protocol, mode, two_look=True)
            for row in plan["compile"]:
                if row["block"] == 14:
                    row["stratum"] = schedule[14]
            path.write_text(json.dumps(plan))
            with self.assertRaisesRegex(ValueError, "plan drifted"):
                evidence._validate_measurement_plan(path, self.protocol, mode, two_look=True)

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

    def test_v3_preserves_the_product_and_controller_as_separate_full_identities(self):
        keys = {"Cargo.lock", "bench.rs"}
        def identity(root, rev):
            return {"root":"C:\\src\\"+root, "revision":rev, "tree":"a"*40,
                    "hashes":{k:"b"*64 for k in keys}}
        controller = identity("controller", "c"*40)
        product = identity("measured", "e"*40)
        reference = identity("reference", self.reference["revision"])
        manifest = {"experiment":{"method":"finite-rank-v3"}, "controller": controller, "measured_subject":product,
                    "control":reference}
        meta = {"controller_revision":controller["revision"], "measured_subject_revision":product["revision"]}
        for mode in timing.MODES:
            subject = product if mode == "candidate-reference" else reference
            inputs = {"controller":controller, "measured_subject":product, "subject":subject, "control":reference}
            self.assertEqual(timing._source_bindings(manifest, inputs, meta, mode, keys), meta)
            for changed in ({k:v for k,v in manifest.items() if k != "control"}, {**manifest, "control":product}):
                with self.assertRaises(ValueError):
                    timing._source_bindings(changed, inputs, meta, mode, keys)
            for bad in ({k:v for k,v in inputs.items() if k!="measured_subject"},
                        {**inputs,"measured_subject":controller}, {**inputs,"subject":controller},
                        {**inputs,"control":product}):
                with self.assertRaises(ValueError):
                    timing._source_bindings(manifest, bad, meta, mode, keys)
            for alias in ("controller_revision", "measured_subject_revision"):
                with self.assertRaises(ValueError):
                    timing._source_bindings(manifest, inputs, {**meta, alias:"1"*40}, mode, keys)
        for bad in ({**product,"tree":0}, {**product,"root":"C:relative"},
                    {**product,"root":"C:\\src\\..\\other"}, {**product,"hashes":{"Cargo.lock":"b"*64}},
                    {**product,"extra":"x"}, {**product,"hashes":dict.fromkeys(keys,"wrong")}):
            with self.subTest(product=bad), self.assertRaises(ValueError):
                timing._source_bindings({**manifest,"measured_subject":bad},
                    {**inputs,"measured_subject":bad,"subject":bad}, meta, "candidate-reference", keys)
        inputs = {"controller":controller, "measured_subject":controller,"subject":controller,"control":reference}
        default_meta = {**meta,"measured_subject_revision":controller["revision"]}
        self.assertEqual(timing._source_bindings({**manifest,"measured_subject":controller}, inputs,
                         default_meta, "candidate-reference", keys), default_meta)
        for old in ({}, {"experiment":{"method":"finite-rank-v2"}}):
            old_manifest = {**old, "controller":controller}
            old_inputs = {k:v for k,v in inputs.items() if k!="measured_subject"}
            self.assertEqual(timing._source_bindings(old_manifest, old_inputs, {}, "candidate-reference", keys), {})
            with self.assertRaises(ValueError):
                timing._source_bindings(old_manifest, {**old_inputs,"subject":product}, {}, "candidate-reference", keys)
            with self.assertRaises(ValueError):
                timing._source_bindings(old_manifest, inputs, {}, "candidate-reference", keys)

    def test_first_release_uses_the_signed_source_for_both_modes(self):
        self.reference["mode"] = "first-release-enrolment"
        files = {"Cargo.lock", "bench.rs"}
        ref = {"root": r"C:\src\reference", "revision": self.reference["revision"],
               "tree": self.reference["tree"], "hashes": {key: "b" * 64 for key in files}}
        measured = {**ref, "root": r"C:\src\subject"}
        manifest = {"experiment": {"method": "finite-rank-v3"}, "controller": measured,
                    "measured_subject": measured, "control": ref}
        meta = {"controller_revision": ref["revision"], "measured_subject_revision": ref["revision"]}
        for mode in timing.MODES:
            inputs = {"controller": measured, "measured_subject": measured, "control": ref,
                      "subject": measured if mode == "candidate-reference" else ref}
            self.assertEqual(timing._source_bindings(manifest, inputs, meta, mode, files), meta)
            for key in files:
                bad = {**ref, "hashes": {**ref["hashes"], key: "c" * 64}}
                with self.subTest(mode=mode, key=key), self.assertRaisesRegex(ValueError, "wrong measured"):
                    timing._source_bindings({**manifest, "control": bad}, {**inputs, "control": bad,
                                            "subject": measured if mode == "candidate-reference" else bad}, meta, mode, files)

    def test_v3_staging_checks_both_revisions_and_original_product_hashes(self):
        tags = {"controller_revision":"c"*40,"measured_subject_revision":"f"*40}
        recorded = {"hashes":{"Cargo.lock":"d"*64,"consumer.rs":"e"*64}}
        staged = {**tags, "original":dict(recorded["hashes"])}
        timing._check_staged_bindings(staged, recorded, tags)
        for bad in ({**staged,"controller_revision":"a"*40}, {**staged,"measured_subject_revision":None},
                    {**staged,"original":{"Cargo.lock":"d"*64}}):
            with self.assertRaises(ValueError):
                timing._check_staged_bindings(bad, recorded, tags)
        with self.assertRaises(ValueError):
            timing._check_staged_bindings(staged, recorded, {})

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
        trace = self.dir / "telemetry/processes.jsonl"
        trace.parent.mkdir()
        for job in timing._jobs(self.protocol, self.counts):
            _, mode, role, work, rel = job
            if not work.startswith("runtime/") or role != "subject":
                continue
            scenario = work.removeprefix("runtime/")
            source = PureWindowsPath("C:/scratch") / mode / f"source-{role}"
            image = source.parent / role / "composition.exe"
            identities = {(mode, role): (source, PureWindowsPath("C:/rust/cargo.exe"), image, None)}
            common = {"destination": str(PureWindowsPath("C:/capture") / rel),
                      "job_memory_limit_bytes": self.protocol["process_memory_limit_bytes"]}
            start = {**common, "phase": "start", "monotonic": 1, "command_kind": "runtime",
                     "timeout_seconds": self.protocol["process_timeout_seconds"], "cwd": str(source)}
            resource = (self.dir / rel).with_suffix(".resources.json")
            resource.parent.mkdir(parents=True)
            for window in ("0.5", "2"):
                args = ["--min-time", window, "--max-time", window]
                command = [str(image), "--bench", "--exact", "composition::"+scenario.replace("/", "::"), *args]
                receipt = {"divan_arguments": args}
                finish = {**common, "phase": "finish", "monotonic": 2, "returncode": 0, "process_receipt": receipt}
                resource.write_text(json.dumps({"process_receipt": receipt, "direct_process": {
                    "CreationTime100ns": 1, "ExitTime100ns": 2}}))
                trace.write_text(json.dumps({**start, "command": command}) + "\n" + json.dumps(finish))
                for experimental in (False, True):
                    with self.subTest(mode=mode, scenario=scenario, window=window, experimental=experimental):
                        if window == "2":
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
            tool = {"path": str(PureWindowsPath("C:/tools") / self.protocol["compiler_telemetry"]["path_suffix"]),
                    "sha256": self.protocol["compiler_telemetry"]["sha256"]}
            roles[mode, role] = source, cargo, source.parent/role/"composition.exe", tool
            target = source.parent/f"compile-direct-01-{role}"
            common = {"destination":str(PureWindowsPath("C:/capture")/rel),
                      "job_memory_limit_bytes": 2*1024**3}
            receipt = {"leader_elapsed_seconds": 12.0+i, "compiler_telemetry_path": tool["path"],
                       "compiler_telemetry_sha256": tool["sha256"]}
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

        def verify(items, *, v3=False):
            trace.write_text("\n".join(json.dumps(x) for x in items))
            timing._verify_processes(self.dir, jobs, roles, self.protocol, experimental_rank=v3)

        verify(events)
        for i, (_, mode, role, _, _) in enumerate(jobs):
            with self.subTest(redirected_mode=mode, redirected_role=role):
                other = {**tool, "path": tool["path"].replace("C:", "D:")}
                original = roles[mode, role]
                roles[mode, role] = (*original[:3], other)
                with self.assertRaisesRegex(ValueError, "enrolled telemetry"):
                    verify(events)
                roles[mode, role] = original
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
        resources["direct_process"] = {"CreationTime100ns": (i - 1) * 20 + 14,
                                       "ExitTime100ns": i * 20 + 15}
        blob.write_text(json.dumps(resources))
        with self.assertRaisesRegex(ValueError, "overlap"):
            verify(events)
        resources["direct_process"] = {"CreationTime100ns": i*20, "ExitTime100ns": i*20+15}
        blob.write_text(json.dumps(resources))
        with self.assertRaisesRegex(ValueError, "build directory"):
            verify(events, v3=True)
        for i, job in enumerate(jobs):
            start, finish = events[2*i:2*i+2]
            start["build_dir"] = start["target"]
            finish["process_receipt"]["build_dir"] = start["target"]
            path = (self.dir / job[-1]).with_suffix(".resources.json")
            path.write_text(json.dumps({"process_receipt": finish["process_receipt"], "direct_process": {
                "CreationTime100ns": i*20, "ExitTime100ns": i*20+15}}))
        verify(events, v3=True)
        with self.assertRaisesRegex(ValueError, "build directory"):
            verify([{**events[0], "build_dir": events[2]["target"]}, *events[1:]], v3=True)
        for redirect in (None, events[2]["target"]):
            path = (self.dir / jobs[0][-1]).with_suffix(".resources.json")
            raw = json.loads(path.read_text())
            if redirect is None:
                raw["process_receipt"].pop("build_dir")
            else:
                raw["process_receipt"]["build_dir"] = redirect
            path.write_text(json.dumps(raw))
            with self.subTest(receipt=redirect), self.assertRaisesRegex(ValueError, "build directory"):
                verify([events[0], {**events[1], "process_receipt":raw["process_receipt"]}, *events[2:]], v3=True)

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
