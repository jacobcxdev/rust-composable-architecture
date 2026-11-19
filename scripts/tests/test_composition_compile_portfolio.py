"""Check the original Windows command, fresh graph and per-fixture cost boundaries."""

import copy
import json
from pathlib import Path, PureWindowsPath
from types import SimpleNamespace
from unittest.mock import patch
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_compile_portfolio as portfolio
import composition_performance as evidence
import test_composition_windows_runner as windows_tests


class CompilePortfolioTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.out = Path(self.temp.name).resolve()
        self.source = PureWindowsPath("C:/src")
        self.targets = PureWindowsPath("C:/build")
        self.capture = PureWindowsPath("C:/capture")
        self.cargo = PureWindowsPath("C:/tools/cargo.exe")
        self.jobs = []
        self.graphs = {}
        for index, stage in enumerate(portfolio.STAGES):
            _, target, command = portfolio._stage(stage, self.cargo, self.source, self.targets)
            resource = windows_tests.WindowsRunnerTests.process_result(compiler=True)
            counters = resource.pop("counters")
            counters.update(CreationTime100ns=200 * index + 100, ExitTime100ns=200 * index + 200)
            resource.update(
                command_kind="compile",
                compiler_telemetry_sha256=evidence.WINDOWS_COMPILER_TELEMETRY["sha256"],
                build_dir=str(target),
            )
            path = self.out / (stage + ".resources.json")
            path.write_text(
                json.dumps(dict(real_seconds=2.0, direct_process=counters, process_receipt=resource))
            )
            destination = str(self.capture / (stage + ".txt"))
            self.jobs.extend(
                [
                    dict(
                        phase="start",
                        monotonic=3 * index + 1.0,
                        destination=destination,
                        command=command,
                        cwd=str(self.source),
                        target=str(target),
                        build_dir=str(target),
                        incremental="0",
                        job_memory_limit_bytes=2 * 1024**3,
                    ),
                    dict(
                        phase="finish",
                        monotonic=3 * index + 2.0,
                        destination=destination,
                        returncode=0,
                        process_receipt=resource,
                        job_memory_limit_bytes=2 * 1024**3,
                    ),
                ]
            )
            manifests = [
                ("rca-core", "rca_core", "lib"),
                ("benches/compile/support", "rca_compile_fixture_support", "lib"),
            ]
            if stage == "composed-macro":
                manifests.extend([("rca", "rca", "lib"), ("rca-macros", "rca_macros", "proc-macro")])
            if stage != "support":
                manifests.append(("benches/compile/" + stage, "rca-compile-" + stage, "bin"))
            rows = []
            graph = set()
            for member, name, kind in manifests:
                folder = self.source / member
                graph.add("local:" + member + "/Cargo.toml")
                rows.append(
                    dict(
                        reason="compiler-artifact",
                        package_id="path+file:///" + member,
                        manifest_path=str(folder / "Cargo.toml"),
                        features=[],
                        filenames=[str(target / (name + ".rmeta"))],
                        target=dict(
                            kind=[kind],
                            name=name,
                            src_path=str(folder / "src" / ("main.rs" if kind == "bin" else "lib.rs")),
                        ),
                        fresh=stage in portfolio.LEAVES and kind != "bin",
                    )
                )
            for name in ("futures-executor", "futures-util"):
                registry_id = portfolio.registry.REGISTRY + "#" + name + "@0.3.34"
                graph.add(registry_id)
                rows.append(
                    dict(
                        reason="compiler-artifact",
                        package_id=registry_id,
                        manifest_path=f"C:/registry/{name}/Cargo.toml",
                        features=[],
                        filenames=[str(target / (name + ".rmeta"))],
                        target=dict(
                            kind=["lib"],
                            name=name.replace("-", "_"),
                            src_path=f"C:/registry/{name}/src/lib.rs",
                        ),
                        fresh=stage in portfolio.LEAVES,
                    )
                )
            self.graphs[stage] = graph
            rows.append(dict(reason="build-finished", success=True))
            (self.out / (stage + ".txt")).write_text("\n".join(map(json.dumps, rows)))

    def replay(self):
        return portfolio.replay(
            self.out,
            self.jobs,
            cargo=self.cargo,
            source=self.source,
            targets=self.targets,
            capture_directory=self.capture,
            graphs=self.graphs,
        )

    def test_one_physical_shared_stage_counts_towards_each_leaf_not_the_macro(self):
        result = self.replay()
        self.assertEqual(sum(result["stages_seconds"].values()), 10.0)
        self.assertEqual(
            result["fixture_seconds"], dict.fromkeys(portfolio.LEAVES, 4.0) | {"composed-macro": 2.0}
        )

    def test_only_the_enrolled_borderline_fixture_is_repeated(self):
        args = dict(
            cargo=self.cargo,
            source=self.source,
            targets=self.targets,
            capture_directory=self.capture,
            graphs=self.graphs,
        )
        result = portfolio.replay(self.out, self.jobs[:4], **args, fixtures=("direct",))
        self.assertEqual(
            result, {"stages_seconds": {"support": 2.0, "direct": 2.0}, "fixture_seconds": {"direct": 4.0}}
        )
        with self.assertRaisesRegex(ValueError, "completeness"):
            portfolio.replay(self.out, self.jobs[:4], **args)
        for invalid in ((), ("support",), ("direct", "direct")):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                portfolio.replay(self.out, [], **args, fixtures=invalid)

    def test_bad_cargo_scope_cannot_report_good_elapsed_time(self):
        for stage, row, field, val in [
            ("support", 0, "fresh", True),
            ("direct", 0, "features", ["different"]),
            ("scoped", 0, "fresh", False),
            ("composed-manual", 2, "fresh", True),
            ("composed-macro", 3, "manifest_path", "C:/other/rca-macros/Cargo.toml"),
            ("support", 1, "filenames", ["C:/build/shared/../macro/fake.rmeta"]),
            ("direct", -1, "success", False),
        ]:
            with self.subTest(stage=stage, field=field):
                log = self.out / (stage + ".txt")
                original = log.read_text()
                records = list(map(json.loads, original.splitlines()))
                records[row][field] = val
                log.write_text("\n".join(map(json.dumps, records)))
                with self.assertRaises(ValueError):
                    self.replay()
                log.write_text(original)

    def test_replay_requires_the_guarded_nonincremental_command_in_the_recorded_order(self):
        good = self.jobs
        for change in ("missing", "reordered", "incremental", "release", "wrong-target", "unclosed", "clock"):
            self.jobs = copy.deepcopy(good)
            if change == "missing":
                self.jobs.pop()
            elif change == "reordered":
                self.jobs[0], self.jobs[2] = self.jobs[2], self.jobs[0]
            elif change == "incremental":
                self.jobs[0]["incremental"] = "1"
            elif change == "release":
                self.jobs[2]["command"].append("--release")
            elif change == "wrong-target":
                self.jobs[0]["build_dir"] = str(self.targets / "macro")
            elif change == "clock":
                self.jobs[2]["monotonic"] = self.jobs[1]["monotonic"] - 0.001
            else:
                self.jobs[1]["process_receipt"]["descendants_exited"] = False
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.replay()

    def test_a_shared_target_does_not_make_two_compilations_the_same_process(self):
        direct = self.out / "direct.resources.json"
        scoped = self.out / "scoped.resources.json"
        scoped.write_bytes(direct.read_bytes())
        with self.assertRaisesRegex(ValueError, "overlap or repeat"):
            self.replay()

    def test_an_omitted_registry_package_does_not_disappear_from_the_signed_graph(self):
        for stage in portfolio.STAGES:
            log = self.out / (stage + ".txt")
            log.write_text(
                "\n".join(line for line in log.read_text().splitlines() if "#futures-executor@" not in line)
            )
        with self.assertRaisesRegex(ValueError, "source, root or shared"):
            self.replay()

    def test_a_local_dependency_cannot_name_source_from_another_checkout(self):
        log = self.out / "support.txt"
        records = list(map(json.loads, log.read_text().splitlines()))
        records[0]["target"]["src_path"] = "C:/another/rca-core/src/lib.rs"
        log.write_text("\n".join(map(json.dumps, records)))
        with self.assertRaisesRegex(ValueError, "another source"):
            self.replay()

    def test_compiler_settings_and_nested_configs_cannot_hide_external_reuse(self):
        for name in (
            "RustFlags",
            "cargo_encoded_rustflags",
            "CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS",
            "cargo_build_rustflags",
            "cargo_profile_dev_incremental",
            "rUsTc_wrAPPer",
        ):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "compiler setting"):
                portfolio._environment(self.out, {name: "-Cincremental=C:/old"})
        source = self.out / "nested"
        source.mkdir()
        (self.out / ".cargo").mkdir()
        (self.out / ".cargo/config.toml").touch()
        with self.assertRaisesRegex(ValueError, "ambient Cargo configuration"):
            portfolio._environment(source, {"CARGO_HOME": str(self.out / "cargo-home")})

    def test_capture_does_not_retire_a_replacement_directory(self):
        targets = self.out / "owned"

        def replace_target(*args):
            targets.rename(self.out / "original")
            targets.mkdir()
            (targets / "sentinel").write_text("not owned")
            raise RuntimeError("simulated interruption")

        monitor = SimpleNamespace(experimental_rank=True, run=replace_target)
        with (
            patch.object(portfolio, "locked_graphs", return_value={}),
            self.assertRaisesRegex(ValueError, "replaced"),
        ):
            portfolio.capture(
                monitor,
                self.cargo,
                self.out,
                self.out / "logs",
                targets,
                {"CARGO_HOME": str(self.out / "cargo-home")},
            )
        self.assertEqual((targets / "sentinel").read_text(), "not owned")

    def test_pr_screen_requires_consistent_orders_and_stable_controls(self):
        def block(candidate, *, control=10):
            return {"base": 10, "candidate": candidate, "control-a": 10, "control-b": control}

        for first, second, decision in (
            (block(10), block(11), "pass"),
            (block(12), block(13), "fail"),
            (block(12), block(10), "inconclusive"),
            (block(12, control=12), block(13), "inconclusive"),
            (block(10), block(10, control=8), "inconclusive"),
        ):
            with self.subTest(first=first, second=second):
                result = portfolio.screen_cold_pairs([first, second])
                self.assertEqual(result["decision"], decision)
                self.assertIs(result["release_authorised"], False)
        for bad in ([block(1)], [block(float("inf")), block(1)], [block(1), {}]):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                portfolio.screen_cold_pairs(bad)

    def test_pr_screen_never_accepts_shared_graph_work_as_standalone(self):
        name = "composed-manual"
        args = (self.out / f"{name}.txt", name, self.source, self.targets / "shared",
                None, self.graphs[name])
        with self.assertRaisesRegex(ValueError, "Cargo reuse"):
            portfolio._units(*args, standalone=True)
        data = [json.loads(line) for line in args[0].read_text().splitlines()]
        for row in data:
            if row["reason"] == "compiler-artifact":
                row["fresh"] = False
        args[0].write_text("\n".join(map(json.dumps, data)))
        self.assertTrue(portfolio._units(*args, standalone=True))
        self.assertEqual(portfolio._ORDERS[0], portfolio._ORDERS[1][::-1])
        self.assertEqual(set(portfolio._ORDERS[0]), {"base", "candidate", "control-a", "control-b"})

    def test_guarded_collection_distinguishes_adverse_results_from_missing_evidence(self):
        flags = ["--base", "base", "--candidate", "subject", "--fixture", "direct", "--output", "results"]
        with patch("builtins.print"), patch.object(portfolio, "capture_pr") as collect:
            for decision, code in (("pass", 0), ("fail", 1), ("inconclusive", 2)):
                collect.return_value = {"decision": decision}
                self.assertEqual(portfolio.main(flags), code)
                self.assertEqual(portfolio.main([*flags, "--collect-only"]), 0)
            collect.side_effect = ValueError("missing measurements")
            self.assertEqual(portfolio.main([*flags, "--collect-only"]), 2)

    def test_confirmation_never_promotes_an_incomplete_pass_or_ignores_bad_controls(self):
        import composition_runner as runner

        protocol = evidence.load_protocol(runner.WINDOWS_PROTOCOL)
        schedule = evidence.seeded_order_schedule(protocol)

        def evaluate(count, candidate, control):
            return portfolio._confirmation_decision({
                mode: [dict(block=n, order=order, subject=ratio, control=1.0)
                       for n, order in enumerate(schedule[:count], 1)]
                for mode, ratio in (("candidate-reference", candidate), ("reference-reference-aa", control))
            }, protocol)

        self.assertIsNone(evaluate(3, 1.3, 1))
        self.assertIsNone(evaluate(19, 1.0, 1))
        self.assertEqual(evaluate(4, 1.3, 1)["decision"], "fail")
        self.assertEqual(evaluate(4, 1.3, 1.2)["decision"], "inconclusive")
        self.assertEqual(evaluate(20, 1, 1)["decision"], "pass")
        self.assertFalse(evaluate(4, 1.3, 1)["controls_complete"])
        self.assertFalse(evaluate(20, 1, 1)["release_authorised"])
