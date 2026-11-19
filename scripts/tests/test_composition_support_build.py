"""Keep combined benchmark preparation isolated from measured builds and from reference diagnostics."""

import hashlib
import json
from pathlib import Path
import runpy
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_support_build as support
from composition_runner import WindowsMonitor


class PreparationTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.calls = []
        self.damage = None
        self.listing = "".join(f"scenario-{i}\n" for i in range(68))
        self.list_hash = hashlib.sha256(self.listing.encode()).hexdigest()

    def prepare(self, mode="candidate-reference", role="subject", *, lock=None, seed=None, env=None,
                cap=2 * 1024**3, monitor_type=WindowsMonitor, complete=True, cargo="cargo", fixed=None):
        group = self.root / "targets" / mode
        src, target = group / ("source-" + role), group / role
        src.mkdir(parents=True)
        (src / "Cargo.lock").write_bytes(b"lock")
        self.target, self.source = target, src
        staging = self.root / "evidence" / mode
        monitor = Mock(spec=monitor_type, run=self.run_command, cargo=cargo, complete=complete,
                       protocol={"process_memory_limit_bytes": cap})
        with patch.object(support, "COMPOSITION_LIST", self.list_hash):
            if fixed is not None:
                return support.prepare_with_fixed_references(src, target, staging, {"cargo":"cargo"}, env or {},
                    monitor, **fixed, lock_sha256=hashlib.sha256(b"lock").hexdigest(),
                    effective_sha256=support.source_digest(src))
            return support.prepare(src, target, staging, {"cargo":"cargo"}, env or {}, monitor,
                                   seed=seed, lock_sha256=lock or hashlib.sha256(b"lock").hexdigest(),
                                   effective_sha256=support.source_digest(src))

    def run_command(self, cmd, log, source, env):
        self.calls.append(cmd)
        if cmd[0] == "cargo":
            self.assertEqual(cmd[cmd.index("-j") + 1], "2")
            self.assertEqual(env["CARGO_TARGET_DIR"], str(self.target))
            self.assertEqual(env["CARGO_BUILD_BUILD_DIR"], str(self.target))
            self.assertEqual(env["CARGO_PROFILE_BENCH_LTO"], "false")
            self.target.mkdir()
            items = []
            for i, arg in enumerate(cmd):
                if arg != "--bench":
                    continue
                name = cmd[i + 1]
                path = self.target / name
                path.write_bytes(name.encode())
                items.append({"reason":"compiler-artifact", "fresh":False, "package_id":"path+file:///rca-core",
                              "manifest_path":str(source / "rca-core/Cargo.toml"), "features":["default"],
                              "target":{"name":name, "kind":["bench"], "src_path":str(source / f"rca-core/benches/{name}.rs")},
                              "profile":dict(support.PROFILE), "filenames":[str(path)], "executable":str(path)})
            if self.damage:
                self.damage(items)
            items.append({"reason":"build-finished","success":True})
            log.write_text("".join(json.dumps(item)+"\n" for item in items))
        elif cmd[1] == "--list":
            log.write_text(self.listing)
        else:
            suite = Path(cmd[0]).name
            text = "suite  fastest │ slowest │ median │ mean │ samples │ iters\n"
            text += "".join(f"├─ {row}    1 ns │ 1 ns │ 1 ns │ 1 ns │ 50 │ 50\n"
                            for row in sorted(support.CORE_ROWS[suite]))
            log.write_text(text)

    def test_four_products_one_command_and_all_27_core_rows(self):
        result = self.prepare()
        self.assertEqual(len(self.calls), 5)
        self.assertEqual(self.calls[0].count("--bench"), 4)
        self.assertEqual(sum(len(p.get("rows", {})) for p in result["executables"].values()), 27)
        self.assertEqual(result["executables"]["composition"]["list_sha256"], self.list_hash)

    def fixed_pair(self):
        import qualification_reference_cache as fixed
        digest = lambda data: hashlib.sha256(data).hexdigest()
        folder = self.root / "cache"
        inputs = {key:digest(key.encode()) for key in fixed.INPUT_HASHES}
        inputs["tree"] = "a" * 40
        roles = {}
        for role in ("a", "b"):
            cache = folder / role
            cache.mkdir(parents=True)
            (cache / "composition.exe").write_bytes(b"reference")
            (cache / "provenance.json").write_bytes(role.encode())
            roles[role] = {"source_root":f"C:/inputs/{role}", "target_root":f"C:/outputs/{role}",
                           "provenance_sha256":digest(role.encode()), "products":{"composition":digest(b"reference")}}
        data = json.dumps({"schema":1, "revision":fixed.RELEASE_BASELINE_REVISION, "purpose":"composition-normal",
                           "inputs":inputs, "roles":roles}).encode()
        (folder / "enrolment.json").write_bytes(data)
        return {"cache":folder, "reference_inputs":inputs, "enrolment_sha256":digest(data)}

    def test_fixed_pair_lists_both_then_builds_candidate_only(self):
        fixed = self.fixed_pair()
        with patch.object(support.registry, "seed_benchmark", side_effect=AssertionError("no donor")):
            result = self.prepare(fixed=fixed)
        self.assertEqual(set(result["roles"]), {"a","b","c"})
        self.assertEqual(self.calls[:2], [[str(fixed["cache"] / r / "composition.exe"), "--list"] for r in "ab"])
        self.assertEqual([cmd[0] for cmd in self.calls].count("cargo"), 1)
        self.assertEqual(self.calls[2].count("--bench"), 4)
        self.assertEqual(sum(len(v.get("rows", {})) for v in result["roles"]["c"].values()), 27)
        self.assertEqual(result["reference_enrolment_sha256"], fixed["enrolment_sha256"])

    def test_substituted_reference_never_starts_a_listing_or_build(self):
        fixed = self.fixed_pair()
        (fixed["cache"] / "b/composition.exe").write_bytes(b"candidate")
        with self.assertRaises(ValueError):
            self.prepare(fixed=fixed)
        self.assertFalse(self.calls)

    def test_wrong_reference_listing_stops_before_candidate_build(self):
        fixed = self.fixed_pair()
        self.list_hash = hashlib.sha256(b"wrong list").hexdigest()
        with self.assertRaisesRegex(ValueError, "list changed"):
            self.prepare(fixed=fixed)
        self.assertEqual(self.calls, [[str(fixed["cache"] / "a/composition.exe"), "--list"]])

    def test_fixed_route_rejects_non_candidate_build_target(self):
        with self.assertRaisesRegex(ValueError, "candidate preparation target"):
            self.prepare(role="control", fixed=self.fixed_pair())
        self.assertFalse(self.calls)

    def test_changed_reference_during_candidate_build_is_rejected(self):
        fixed = self.fixed_pair()
        self.damage = lambda _: (fixed["cache"] / "a/composition.exe").write_bytes(b"replacement")
        with self.assertRaises(ValueError):
            self.prepare(fixed=fixed)

    def test_control_and_both_reference_arms_build_composition_only(self):
        for mode, role in (("candidate-reference", "control"), ("reference-reference-aa", "control"),
                           ("reference-reference-aa", "subject")):
            self.calls.clear()
            result = self.prepare(mode, role)
            self.assertEqual(set(result["executables"]), {"composition"})
            self.assertEqual(self.calls[0].count("--bench"), 1)

    def test_missing_duplicate_reused_foreign_and_wrong_profile_outputs_fail(self):
        failures = (
            lambda rows: rows.pop(),
            lambda rows: rows.append(rows[-1]),
            lambda rows: rows[1].update(fresh=True),
            lambda rows: rows[1].update(executable=str(self.root / "out"), filenames=[str(self.root / "out")]),
            lambda rows: rows[1].update(manifest_path=str(self.root / "other/Cargo.toml")),
            lambda rows: rows[1].update(profile={**support.PROFILE, "debuginfo":2}),
        )
        for number, damage in enumerate(failures):
            with self.subTest(number=number):
                self.root = self.root / str(number)
                self.damage = damage
                with self.assertRaises(ValueError):
                    self.prepare()

    def test_changed_list_same_count_and_changed_executable_fail(self):
        self.list_hash = hashlib.sha256(b"wrong list").hexdigest()
        with self.assertRaisesRegex(ValueError, "list changed"):
            self.prepare()
        self.root = self.root / "next"
        self.list_hash = hashlib.sha256(self.listing.encode()).hexdigest()
        run = self.run_command
        def changed(cmd, *args):
            run(cmd, *args)
            if cmd[1] == "--list":
                Path(cmd[0]).write_bytes(b"another executable")
        with patch.object(self, "run_command", changed), self.assertRaisesRegex(ValueError, "changed during"):
            self.prepare()

    def test_wrong_lock_or_timed_target_never_starts_cargo(self):
        with self.assertRaises(ValueError):
            self.prepare(lock="0"*64)
        with self.assertRaises(ValueError):
            self.prepare(role="compile-scoped-01")
        self.assertFalse(self.calls)

    def test_wrong_memory_limit_or_cargo_monitor_never_starts_cargo(self):
        for i, options in enumerate(({"cap": None}, {"cap": 4 * 1024**3}, {"complete": False}, {"cargo": "other"})):
            with self.subTest(options=options):
                self.root = self.root / str(i)
                with self.assertRaisesRegex(ValueError, "2 GiB"):
                    self.prepare(**options)
        self.assertFalse(self.calls)

    def test_script_runner_monitor_uses_the_same_job_settings(self):
        runner = Path(__file__).resolve().parents[1] / "composition_runner.py"
        script_monitor = runpy.run_path(str(runner))["WindowsMonitor"]
        self.assertIsNot(script_monitor, WindowsMonitor)
        self.prepare(monitor_type=script_monitor)

    def test_seed_uses_bound_target_and_lock_and_cannot_seed_donor(self):
        seed = object()
        with patch.object(support.registry, "seed_benchmark", return_value={}) as copy:
            self.prepare(seed=seed)
            copy.assert_called_once_with(seed, self.target, lock_sha256=hashlib.sha256(b"lock").hexdigest())
        count = len(self.calls)
        with self.assertRaisesRegex(ValueError, "new, isolated"):
            self.prepare(role="control", seed=seed)
        self.assertEqual(len(self.calls), count)

    def test_different_profile_or_diagnostic_override_never_starts(self):
        for index, env in enumerate(({"CARGO_PROFILE_BENCH_LTO":"true"}, {"DIVAN_SAMPLE_COUNT":"1"})):
            self.root = self.root / str(index)
            with self.assertRaisesRegex(ValueError, "override"):
                self.prepare(env=env)
        self.assertFalse(self.calls)

    def test_changed_row_set_cannot_pass_with_same_count(self):
        with patch.object(support.evidence, "parse_divan_transcript",
                          return_value=dict.fromkeys(("other", "one", "two", "three"), 1.0)):
            with self.assertRaisesRegex(ValueError, "diagnostic rows"):
                support.read_diagnostic("store_actor", self.root / "transcript")
