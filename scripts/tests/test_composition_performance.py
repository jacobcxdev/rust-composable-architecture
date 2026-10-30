import importlib.util
import json
import math
import os
import pathlib
import subprocess
import tempfile
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
MODULE_PATH = REPO_ROOT / "scripts" / "composition_performance.py"
SPEC = importlib.util.spec_from_file_location("composition_performance", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
composition_performance = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(composition_performance)


class ComposableFiniteStreamTests(unittest.TestCase):
    def rows(self, engine="rca"):
        return [dict(schema="composable-finite-stream-v1", engine=engine,
                     case=case, expected=values, observed=list(values),
                     terminal_reduced=True, producer_exhausted=True,
                     stream_drops=1, state_extracted=True, host_stopped=True)
                for case, values in (("empty", []), ("ordered", [3, 1, 4, 2]))]

    def test_accepts_both_complete_traces_for_each_engine(self):
        for engine in ("composable", "rca"):
            rows = self.rows(engine)
            self.assertIs(composition_performance.validate_composable_stream_work(rows, engine), rows)

    def test_rejects_partial_reordered_mistyped_and_wrong_engine_evidence(self):
        invalid = [None, {}, self.rows()[:1], self.rows()[::-1], self.rows() * 2,
                   [None, self.rows()[1]]]
        for field, value in (("engine", "composable"), ("case", "empty"),
                             ("expected", [1, 2, 3, 4]), ("observed", [1, 2, 3, 4]),
                             ("observed", [3, 1, 4, 4]), ("observed", [3, True, 4, 2]),
                             ("stream_drops", True), ("stream_drops", 0), ("stream_drops", 2),
                             ("producer_exhausted", False), ("terminal_reduced", 1),
                             ("state_extracted", False), ("host_stopped", False)):
            rows = self.rows()
            rows[1][field] = value
            invalid.append(rows)
        for field in self.rows()[0]:
            rows = self.rows()
            del rows[0][field]
            invalid.append(rows)
        extra = self.rows()
        extra[0]["timing_ns"] = 1
        invalid.append(extra)
        for rows in invalid:
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                composition_performance.validate_composable_stream_work(rows, "rca")
        for engine in ("unknown", None, True):
            with self.subTest(engine=engine), self.assertRaises(ValueError):
                composition_performance.validate_composable_stream_work(self.rows(), engine)
        with self.assertRaises(ValueError):
            composition_performance.validate_composable_stream_work(self.rows("rca-0.7"), "rca-0.7")

    def test_capture_requires_successful_process_and_complete_output(self):
        output = "\n".join(json.dumps(row) for row in self.rows())
        command = [os.sys.executable, "-c", f"print({output!r})"]
        self.assertEqual(composition_performance.capture_composable_stream_work(command, "rca"), self.rows())
        with self.assertRaises(subprocess.CalledProcessError):
            composition_performance.capture_composable_stream_work(
                [os.sys.executable, "-c", f"print({output!r}); raise SystemExit(7)"], "rca")
        for bad in (output.splitlines()[0], output + "\n{}", "not json"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                composition_performance.capture_composable_stream_work(
                    [os.sys.executable, "-c", f"print({bad!r})"], "rca")


class ComposableReadyFutureTests(unittest.TestCase):
    def ledger(self, engine="rca"):
        return dict(schema="composable-ready-future-v1", engine=engine,
                    operations_expected=64, operations_attempted=64,
                    operations_completed=64, results=64, checksum=6496,
                    future_polls=64, future_drops=64, errors=0,
                    state_extracted=True, host_stopped=True,
                    completion="causal-finish" if engine == "rca" else "result-acknowledgement")

    def test_accepts_engine_specific_completion_without_mutation(self):
        for engine in ("rca", "composable"):
            row = self.ledger(engine)
            before = json.dumps(row, sort_keys=True)
            self.assertIs(composition_performance.validate_composable_ready_future_work(row, engine), row)
            self.assertEqual(json.dumps(row, sort_keys=True), before)

    def test_rejects_missing_extra_wrong_typed_or_incomplete_evidence(self):
        invalid = [None, [], "ledger", self.ledger("composable")]
        for field, required in self.ledger().items():
            row = self.ledger()
            del row[field]
            invalid.append(row)
            for bad in (None, False, 0, "wrong", 64.0):
                if type(bad) is type(required) and bad == required:
                    continue
                row = self.ledger()
                row[field] = bad
                invalid.append(row)
        invalid.extend((dict(self.ledger(), timing_ns=1),
                        dict(self.ledger(), completion="result-acknowledgement")))
        for row in invalid:
            with self.subTest(row=row), self.assertRaises(ValueError):
                composition_performance.validate_composable_ready_future_work(row, "rca")
        for engine in (None, True, "unknown"):
            with self.subTest(engine=engine), self.assertRaises(ValueError):
                composition_performance.validate_composable_ready_future_work(self.ledger(), engine)
        with self.assertRaises(ValueError):
            composition_performance.validate_composable_ready_future_work(self.ledger("rca-0.7"), "rca-0.7")
        with self.assertRaises(ValueError):
            composition_performance.validate_composable_ready_future_work(
                dict(self.ledger("composable"), completion="causal-finish"), "composable")

    def test_capture_requires_success_and_exactly_one_valid_document(self):
        output = json.dumps(self.ledger())
        command = [os.sys.executable, "-c", f"print({output!r})"]
        self.assertEqual(composition_performance.capture_composable_ready_future_work(command, "rca"), self.ledger())
        with self.assertRaises(subprocess.CalledProcessError):
            composition_performance.capture_composable_ready_future_work(
                [os.sys.executable, "-c", f"print({output!r}); raise SystemExit(7)"], "rca")
        duplicate = '{"operations_completed":0,' + output[1:]
        for bad in ("", "not json", output + "\n" + output,
                    json.dumps(self.ledger("composable")), duplicate):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                composition_performance.capture_composable_ready_future_work(
                    [os.sys.executable, "-c", f"print({bad!r})"], "rca")


class ReadyFutureAllocationTests(unittest.TestCase):
    def ledger(self, engine="rca", scale=1):
        snapshots = {
            name: dict(allocated_blocks=blocks * scale, allocated_bytes=size * scale,
                       live_blocks=live * scale, live_bytes=live_size * scale)
            for name, blocks, size, live, live_size in (
                ("A", 0, 0, 0, 0), ("B", 2, 16, 2, 16), ("C", 5, 40, 3, 24),
                ("D", 49, 392, 4, 32), ("E", 50, 400, 0, 0))
        }
        phases = {name: dict(operations=operations, allocated_blocks=blocks * scale,
                             allocated_bytes=size * scale)
                  for name, operations, blocks, size in (
                      ("construction", 1, 2, 16), ("initial", 20, 3, 24),
                      ("checked", 44, 44, 352), ("teardown", 1, 1, 8), ("setup", 1, 5, 40))}
        return dict(schema="composable-ready-future-allocations-v1", engine=engine,
                    instrument="dhat-0.3.3", work=ComposableReadyFutureTests().ledger(engine),
                    snapshots=snapshots, phases=phases)

    def validate(self, value, engine="rca"):
        return composition_performance.validate_composable_ready_future_allocations(value, engine)

    def test_accepts_all_engines_zero_allocations_and_falling_live_counts(self):
        self.assertTrue(callable(getattr(composition_performance,
                                        "validate_composable_ready_future_allocations", None)),
                        "ready-future allocation admission missing")
        for engine in ("rca", "composable"):
            for scale in (0, 1):
                value = self.ledger(engine, scale)
                before = json.dumps(value, sort_keys=True)
                self.assertIs(self.validate(value, engine), value)
                self.assertEqual(json.dumps(value, sort_keys=True), before)

    def test_rejects_missing_extra_and_mistyped_fields_at_each_level(self):
        for path in ((), ("work",), ("snapshots",), ("snapshots", "A"),
                     ("phases",), ("phases", "checked")):
            for change in ("missing", "extra", "list"):
                value = self.ledger()
                target = value
                for key in path:
                    target = target[key]
                if change == "missing":
                    del target[next(iter(target))]
                elif change == "extra":
                    target["extra"] = 0
                elif path:
                    parent = value
                    for key in path[:-1]:
                        parent = parent[key]
                    parent[path[-1]] = []
                else:
                    value = []
                with self.subTest(path=path, change=change), self.assertRaises(ValueError):
                    self.validate(value)

    def test_rejects_invalid_counts_deltas_identity_and_completion(self):
        for group in ("snapshots", "phases"):
            for name, fields in self.ledger()[group].items():
                for field in fields:
                    for bad in (True, -1, 0.5, "1", None, 2**64):
                        value = self.ledger()
                        value[group][name][field] = bad
                        with self.subTest(group=group, name=name, field=field, bad=bad):
                            with self.assertRaises(ValueError):
                                self.validate(value)
        for field in ("schema", "instrument", "engine"):
            value = self.ledger()
            value[field] = "wrong"
            with self.assertRaises(ValueError):
                self.validate(value)
        for field in ("engine", "completion", "host_stopped", "checksum", "future_drops"):
            value = self.ledger()
            value["work"][field] = "wrong"
            with self.assertRaises(ValueError):
                self.validate(value)
        for name, fields in self.ledger()["phases"].items():
            for field in fields:
                value = self.ledger()
                value["phases"][name][field] += 1
                with self.assertRaises(ValueError):
                    self.validate(value)
        for field in ("allocated_blocks", "allocated_bytes"):
            value = self.ledger()
            value["snapshots"]["D"][field] = 0
            with self.assertRaises(ValueError):
                self.validate(value)
        for field in ("live_blocks", "live_bytes"):
            value = self.ledger()
            value["snapshots"]["A"][field] = 1
            with self.assertRaises(ValueError):
                self.validate(value)
        for engine in (None, True, "unknown"):
            with self.assertRaises(ValueError):
                self.validate(self.ledger(), engine)
        with self.assertRaises(ValueError):
            self.validate(self.ledger("rca-0.7"), "rca-0.7")

    def test_capture_rejects_duplicate_fields_failed_processes_and_multiple_documents(self):
        output = json.dumps(self.ledger())
        capture = composition_performance.capture_composable_ready_future_allocations
        command = [os.sys.executable, "-c", f"print({output!r})"]
        self.assertEqual(capture(command, "rca"), self.ledger())
        for bad in ("", "not json", output + "\n" + output,
                    output.replace('"checked":', '"checked":{},"checked":'),
                    output.replace('"checksum":', '"checksum":0,"checksum":'),
                    json.dumps(self.ledger("composable"))):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                capture([os.sys.executable, "-c", f"print({bad!r})"], "rca")
        with self.assertRaises(subprocess.CalledProcessError):
            capture([os.sys.executable, "-c", f"print({output!r}); raise SystemExit(7)"], "rca")


class ComposableCounterWorkTests(unittest.TestCase):
    def ledger(self, engine: str = "composable") -> dict:
        return {
            "schema": "composable-counter-work-v1",
            "engine": engine,
            "warmup_expected": 20,
            "warmup_attempted": 20,
            "warmup_completed": 20,
            "operations_expected": 10000,
            "operations_attempted": 10000,
            "operations_completed": 10000,
            "initial": 0,
            "final": 10020,
            "errors": 0,
            "state_extracted": True,
            "host_stopped": True,
        }

    def test_accepts_exact_completed_work_for_all_engines(self):
        for engine in ("composable", "rca"):
            with self.subTest(engine=engine):
                ledger = self.ledger(engine)
                self.assertIs(
                    composition_performance.validate_composable_counter_work(ledger),
                    ledger,
                )

    def test_rejects_missing_and_extra_fields(self):
        for field in self.ledger():
            with self.subTest(missing=field):
                ledger = self.ledger()
                del ledger[field]
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_work(ledger)
        ledger = self.ledger()
        ledger["timing_ns"] = 1
        with self.assertRaises(ValueError):
            composition_performance.validate_composable_counter_work(ledger)

    def test_rejects_schema_engine_counts_and_boundary_drift(self):
        for field, bad in (
            ("schema", "composable-counter-work-v2"),
            ("engine", "unknown"),
            ("engine", "rca-0.7"),
            ("warmup_expected", 19),
            ("warmup_attempted", 19),
            ("warmup_completed", 19),
            ("operations_expected", 9999),
            ("operations_attempted", 9999),
            ("operations_completed", 9999),
            ("initial", 1),
            ("final", 10019),
            ("errors", 1),
            ("state_extracted", False),
            ("host_stopped", False),
        ):
            with self.subTest(field=field):
                ledger = self.ledger()
                ledger[field] = bad
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_work(ledger)

    def test_rejects_omitted_duplicate_and_misclassified_actions(self):
        for field, value in (
            ("operations_attempted", 9999),
            ("operations_attempted", 10001),
            ("operations_completed", 9999),
            ("operations_completed", 10001),
            ("warmup_attempted", 19),
            ("warmup_completed", 21),
            ("final", 10019),
            ("final", 10021),
        ):
            with self.subTest(field=field, value=value):
                ledger = self.ledger()
                ledger[field] = value
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_work(ledger)

    def test_rejects_mistyped_counts_and_boundaries(self):
        numeric_fields = (
            "warmup_expected", "warmup_attempted", "warmup_completed",
            "operations_expected", "operations_attempted", "operations_completed",
            "initial", "final", "errors",
        )
        for field in numeric_fields:
            for bad in (True, str(self.ledger()[field]), float(self.ledger()[field])):
                with self.subTest(field=field, bad=bad):
                    ledger = self.ledger()
                    ledger[field] = bad
                    with self.assertRaises(ValueError):
                        composition_performance.validate_composable_counter_work(ledger)
        for field in ("state_extracted", "host_stopped"):
            for bad in (1, "true"):
                with self.subTest(field=field, bad=bad):
                    ledger = self.ledger()
                    ledger[field] = bad
                    with self.assertRaises(ValueError):
                        composition_performance.validate_composable_counter_work(ledger)

    def test_rejects_non_object(self):
        for value in (None, [], "{}"):
            with self.subTest(value=value):
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_work(value)

    def test_capture_rejects_valid_ledger_from_failed_process(self):
        ledger = json.dumps(self.ledger())
        command = [
            os.sys.executable,
            "-c",
            f"import sys; print({ledger!r}); sys.exit(7)",
        ]

        with self.assertRaises(subprocess.CalledProcessError) as failure:
            composition_performance.capture_composable_counter_work(command)

        self.assertEqual(failure.exception.returncode, 7)

    def test_capture_validates_successful_process_output(self):
        ledger = json.dumps(self.ledger("rca"))
        command = [os.sys.executable, "-c", f"print({ledger!r})"]

        self.assertEqual(
            composition_performance.capture_composable_counter_work(command),
            self.ledger("rca"),
        )


class ComposableCounterTimingTests(unittest.TestCase):
    def measurement(self):
        return {
            "schema": "composable-counter-timing-v1",
            "clock": "std::time::Instant",
            "unit": "ns",
            "work": ComposableCounterWorkTests().ledger(),
            "loop_ns": 20_000,
            "samples_ns": [1] * 10_000,
        }

    def test_accepts_complete_raw_measurement(self):
        value = self.measurement()
        self.assertIs(composition_performance.validate_composable_counter_timing(value), value)

    def test_rejects_incomplete_work_even_with_complete_samples(self):
        for field, invalid in (("operations_completed", 9999), ("final", 10019),
                               ("host_stopped", False), ("state_extracted", False)):
            with self.subTest(field=field):
                value = self.measurement()
                value["work"][field] = invalid
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_timing(value)

    def test_rejects_missing_extra_or_mistyped_timing(self):
        cases = [None, [], "{}"]
        for field in self.measurement():
            value = self.measurement()
            del value[field]
            cases.append(value)
        value = self.measurement()
        value["qualified"] = True
        cases.append(value)
        for field, invalid in (
            ("schema", "other"), ("clock", "wall"), ("unit", "us"),
            ("loop_ns", 0), ("loop_ns", -1), ("loop_ns", True),
            ("loop_ns", 1.0), ("loop_ns", "20000"), ("loop_ns", 2**128),
            ("samples_ns", [1] * 9999), ("samples_ns", [1] * 10001),
            ("samples_ns", None), ("samples_ns", "1"),
        ):
            value = self.measurement()
            value[field] = invalid
            cases.append(value)
        for invalid in (-1, True, 1.0, "1", None, 2**128):
            value = self.measurement()
            value["samples_ns"][100] = invalid
            cases.append(value)
        for value in cases:
            with self.subTest(value_type=type(value).__name__):
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_timing(value)

    def test_loop_must_cover_sum_of_nonoverlapping_samples(self):
        value = self.measurement()
        value["loop_ns"] = 9999
        with self.assertRaises(ValueError):
            composition_performance.validate_composable_counter_timing(value)
        value["loop_ns"] = 10000
        self.assertIs(composition_performance.validate_composable_counter_timing(value), value)

    def test_zero_resolution_samples_are_not_replaced_with_pseudocounts(self):
        value = self.measurement()
        value["samples_ns"] = [0] * 10_000
        self.assertEqual(
            composition_performance.validate_composable_counter_timing(value)["samples_ns"],
            [0] * 10_000,
        )

    def test_capture_requires_success_and_expected_engine(self):
        value = self.measurement()
        encoded = json.dumps(value)
        for code, engine, error in (
            (7, "composable", subprocess.CalledProcessError),
            (0, "rca", ValueError),
            (0, "rca-0.7", ValueError),
        ):
            command = [os.sys.executable, "-c", f"import sys; print({encoded!r}); sys.exit({code})"]
            with self.subTest(code=code, engine=engine), self.assertRaises(error):
                composition_performance.capture_composable_counter_timing(command, engine)
        command = [os.sys.executable, "-c", f"print({encoded!r})"]
        self.assertEqual(
            composition_performance.capture_composable_counter_timing(command, "composable"), value
        )


class ComposableCounterLifecycleTests(unittest.TestCase):
    def measurement(self, engine="composable"):
        return {
            "schema": "composable-counter-lifecycle-v1",
            "clock": "std::time::Instant",
            "unit": "ns",
            "engine": engine,
            "operations_expected": 1,
            "operations_attempted": 1,
            "operations_completed": 1,
            "initial": 0,
            "final": 1,
            "errors": 0,
            "state_extracted": True,
            "host_stopped": True,
            "elapsed_ns": 123,
        }

    def test_accepts_complete_lifecycle_for_all_engines_and_duration_bounds(self):
        for engine in ("composable", "rca"):
            for elapsed in (1, 2**128 - 1):
                with self.subTest(engine=engine, elapsed=elapsed):
                    value = self.measurement(engine)
                    value["elapsed_ns"] = elapsed
                    self.assertIs(
                        composition_performance.validate_composable_counter_lifecycle(value), value
                    )

    def test_rejects_non_object_missing_extra_and_steady_state_fields(self):
        cases = [None, [], "{}", ComposableCounterTimingTests().measurement()]
        for field in self.measurement():
            value = self.measurement()
            del value[field]
            cases.append(value)
        for field in ("warmup_expected", "work", "qualified"):
            value = self.measurement()
            value[field] = 0
            cases.append(value)
        for value in cases:
            with self.subTest(value=value), self.assertRaises(ValueError):
                composition_performance.validate_composable_counter_lifecycle(value)

    def test_rejects_schema_engine_counts_state_and_teardown_drift(self):
        for field, invalid in (
            ("schema", "composable-counter-timing-v1"),
            ("clock", "wall"), ("unit", "us"), ("engine", "unknown"),
            ("engine", "rca-0.7"),
            ("operations_expected", 0), ("operations_expected", 2),
            ("operations_attempted", 0), ("operations_attempted", 2),
            ("operations_completed", 0), ("operations_completed", 2),
            ("initial", 1), ("final", 0), ("final", 2), ("errors", 1),
            ("state_extracted", False), ("host_stopped", False),
            ("elapsed_ns", 0), ("elapsed_ns", -1), ("elapsed_ns", 2**128),
        ):
            with self.subTest(field=field, invalid=invalid):
                value = self.measurement()
                value[field] = invalid
                with self.assertRaises(ValueError):
                    composition_performance.validate_composable_counter_lifecycle(value)

    def test_rejects_mistyped_fields(self):
        for field, required in self.measurement().items():
            if type(required) is int:
                invalid_values = (True, float(required), str(required), None)
            elif type(required) is bool:
                invalid_values = (1, "true", None)
            else:
                invalid_values = (1, True, None, [])
            for invalid in invalid_values:
                with self.subTest(field=field, invalid=invalid):
                    value = self.measurement()
                    value[field] = invalid
                    with self.assertRaises(ValueError):
                        composition_performance.validate_composable_counter_lifecycle(value)

    def test_capture_requires_success_and_expected_engine(self):
        value = self.measurement()
        encoded = json.dumps(value)
        for code, engine, error in (
            (7, "composable", subprocess.CalledProcessError),
            (0, "rca", ValueError), (0, "unknown", ValueError),
            (0, "rca-0.7", ValueError),
        ):
            command = [os.sys.executable, "-c", f"import sys; print({encoded!r}); sys.exit({code})"]
            with self.subTest(code=code, engine=engine), self.assertRaises(error):
                composition_performance.capture_composable_counter_lifecycle(command, engine)
        for engine in ("composable", "rca"):
            value = self.measurement(engine)
            encoded = json.dumps(value)
            command = [os.sys.executable, "-c", f"print({encoded!r})"]
            self.assertEqual(
                composition_performance.capture_composable_counter_lifecycle(command, engine), value
            )

    def test_capture_rejects_malformed_and_incomplete_output(self):
        for output in ("not json", "{}", json.dumps(ComposableCounterTimingTests().measurement())):
            command = [os.sys.executable, "-c", f"print({output!r})"]
            with self.subTest(output=output), self.assertRaises(ValueError):
                composition_performance.capture_composable_counter_lifecycle(command, "composable")


def write_isolated_observation(
    root: pathlib.Path,
    scenario: str,
    block: int,
    nanoseconds: float,
) -> None:
    scenario_directory = root / "runtime" / scenario.replace("/", "__")
    scenario_directory.mkdir(parents=True, exist_ok=True)
    components = scenario.split("/")
    header = "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
    if len(components) == 1:
        body = f"╰─ {components[0]}  1 ns │ 2 ns │ {nanoseconds} ns │ 1 ns │ 50 │ 50\n"
    else:
        body = (
            f"├─ {components[0]}                                    │        │        │        │    │\n"
            f"│  ╰─ {components[1]}  1 ns │ 2 ns │ {nanoseconds} ns │ 1 ns │ 50 │ 50\n"
        )
    transcript = scenario_directory / f"block-{block:02d}.txt"
    transcript.write_text(header + body, encoding="utf-8")
    resources = {
        "real_seconds": 0.5,
        "user_seconds": 0.4,
        "system_seconds": 0.1,
        "maximum_resident_set_bytes": 1000,
        "page_reclaims": 1,
        "page_faults": 0,
        "swaps": 0,
        "block_input_operations": 0,
        "block_output_operations": 0,
        "voluntary_context_switches": 1,
        "involuntary_context_switches": 0,
        "instructions_retired": 1000,
        "cycles_elapsed": 2000,
        "peak_memory_footprint_bytes": 900,
    }
    transcript.with_suffix(".resources.json").write_text(
        json.dumps(resources), encoding="utf-8"
    )


def write_compile_observation(
    root: pathlib.Path, fixture: str, block: int, real_seconds: float
) -> None:
    fixture_directory = root / "compile" / fixture
    fixture_directory.mkdir(parents=True, exist_ok=True)
    transcript = fixture_directory / f"block-{block:02d}.txt"
    transcript.write_text("Finished release target\n", encoding="utf-8")
    resources = {
        "real_seconds": real_seconds,
        "user_seconds": real_seconds * 0.8,
        "system_seconds": real_seconds * 0.2,
        "maximum_resident_set_bytes": 1000,
        "page_reclaims": 1,
        "page_faults": 0,
        "swaps": 0,
        "block_input_operations": 0,
        "block_output_operations": 0,
        "voluntary_context_switches": 1,
        "involuntary_context_switches": 0,
        "instructions_retired": 1000,
        "cycles_elapsed": 2000,
        "peak_memory_footprint_bytes": 900,
    }
    transcript.with_suffix(".resources.json").write_text(
        json.dumps(resources), encoding="utf-8"
    )


class DivanTranscriptTests(unittest.TestCase):
    def test_parses_complete_task_4_transcript(self):
        transcript = REPO_ROOT / "scripts/tests/fixtures/composition-performance/raw/runtime-divan-01.txt"

        rows = composition_performance.parse_divan_transcript(transcript)

        self.assertEqual(len(rows), 153)
        self.assertEqual(rows["direct/1"], 6_937)
        self.assertEqual(rows["combine_arity/12"], 9_041)
        self.assertEqual(rows["scoped_concurrent_senders_32"], 216_200)

    def test_rejects_duplicate_timing_rows(self):
        header = (
            "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
        )
        line = (
            "├─ direct  1 ns │ 2 ns │ 1 ns │ 1 ns │ 50 │ 50\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "duplicate.txt"
            transcript.write_text(header + line + line, encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "duplicate Divan row"):
                composition_performance.parse_divan_transcript(transcript)


class PairedComparisonTests(unittest.TestCase):
    def _write_protocol_pair_fixture(
        self,
        root: pathlib.Path,
        protocol: dict,
        mode: str,
    ) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
        subject = root / "subject"
        control = root / "control"
        for scenario in protocol["runtime"]["gated_scenarios"]:
            for block in range(1, 21):
                write_isolated_observation(subject, scenario, block, 100.0)
                write_isolated_observation(control, scenario, block, 100.0)
        for fixture in protocol["compile"]["fixtures"]:
            for block in range(1, 21):
                write_compile_observation(subject, fixture, block, 6.0)
                write_compile_observation(control, fixture, block, 6.0)
        plan = root / "measurement-plan.json"
        plan.write_text(
            json.dumps(composition_performance.measurement_plan(protocol, mode)),
            encoding="utf-8",
        )
        return subject, control, plan

    def test_frozen_protocol_has_balanced_seeded_order(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )

        schedule = composition_performance.seeded_order_schedule(protocol)

        self.assertEqual(len(schedule), 20)
        self.assertEqual(schedule.count("control-subject"), 10)
        self.assertEqual(schedule.count("subject-control"), 10)
        self.assertEqual(
            schedule[:6],
            [
                "subject-control",
                "subject-control",
                "control-subject",
                "control-subject",
                "subject-control",
                "control-subject",
            ],
        )
        self.assertNotEqual(
            schedule,
            [
                "control-subject" if block % 2 else "subject-control"
                for block in range(1, 21)
            ],
        )

    def test_distribution_free_upper_bound_exceeds_95_percent_joint_coverage(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        repetitions = []
        values = {
            "control-subject": [1.00, 1.01, 1.02, 1.03, 1.04, 1.05, 1.06, 1.07, 1.08, 1.09],
            "subject-control": [0.99, 1.00, 1.01, 1.02, 1.03, 1.04, 1.05, 1.06, 1.07, 1.08],
        }
        offsets = {order: 0 for order in values}
        for block, order in enumerate(schedule, start=1):
            ratio = values[order][offsets[order]]
            offsets[order] += 1
            repetitions.append(
                {
                    "block": block,
                    "order": order,
                    "control": 100.0,
                    "subject": 100.0 * ratio,
                }
            )

        result = composition_performance.paired_ratio_statistics(
            repetitions, "maximum", protocol
        )

        self.assertTrue(math.isclose(result["point_estimate"], math.sqrt(1.045 * 1.035)))
        self.assertTrue(math.isclose(result["upper_confidence_bound"], math.sqrt(1.08 * 1.07)))
        self.assertEqual(result["confidence_level"], 0.95)
        self.assertEqual(result["finite_sample_joint_coverage"], 1002 / 1024)

    def test_upper_bound_rejects_ten_percent_point_estimate_with_noisy_tail(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        repetitions = []
        offsets = {order: 0 for order in set(schedule)}
        strata = {
            "control-subject": [1.0] * 8 + [1.11, 1.50],
            "subject-control": [1.0] * 8 + [1.11, 1.40],
        }
        for block, order in enumerate(schedule, start=1):
            ratio = strata[order][offsets[order]]
            offsets[order] += 1
            repetitions.append(
                {
                    "block": block,
                    "order": order,
                    "control": 100.0,
                    "subject": 100.0 * ratio,
                }
            )

        result = composition_performance.paired_ratio_statistics(
            repetitions, "maximum", protocol
        )

        self.assertEqual(result["point_estimate"], 1.0)
        self.assertGreater(result["upper_confidence_bound"], 1.10)
        self.assertFalse(result["passed"])

    def test_upper_bound_rejects_missing_order_stratum_observation(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        repetitions = [
            {
                "block": block,
                "order": order,
                "control": 100.0,
                "subject": 100.0,
            }
            for block, order in enumerate(schedule, start=1)
        ]

        with self.assertRaisesRegex(ValueError, "twenty seeded blocks"):
            composition_performance.paired_ratio_statistics(
                repetitions[:-1], "maximum", protocol
            )

    def test_measurement_plan_is_frozen_before_results(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )

        plan = composition_performance.measurement_plan(
            protocol, "candidate-reference"
        )

        self.assertEqual(plan["method_version"], "composition-paired-v5")
        self.assertEqual(plan["protocol_sha256"], composition_performance.PROTOCOL_SHA256)
        self.assertEqual(len(plan["runtime"]), 20 * 9)
        self.assertEqual(len(plan["compile"]), 20 * 4)
        self.assertEqual(plan["runtime"][0]["roles"], ["candidate", "reference"])
        self.assertEqual(plan["runtime"][1]["scenario"], "nonmatching_scope/16")

    def test_aa_plan_uses_two_independent_reference_roles(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )

        plan = composition_performance.measurement_plan(
            protocol, "reference-reference-aa"
        )

        self.assertEqual(plan["runtime"][0]["roles"], ["reference-b", "reference-a"])
        self.assertNotIn("candidate", json.dumps(plan))

    def test_isolated_transcript_requires_exactly_the_gated_scenario(self):
        header = "composition  fastest │ slowest │ median │ mean │ samples │ iters\n"
        transcript_text = (
            header
            + "├─ direct                                    │        │        │        │    │\n"
            + "│  ╰─ 1  1 ns │ 2 ns │ 1.05 ns │ 1 ns │ 50 │ 50\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            transcript = pathlib.Path(directory) / "isolated.txt"
            transcript.write_text(transcript_text, encoding="utf-8")

            rows = composition_performance.validate_isolated_transcript(
                transcript, "direct/1"
            )

            self.assertEqual(rows, {"direct/1": 1.05})
            with self.assertRaisesRegex(ValueError, "isolated Divan scenario"):
                composition_performance.validate_isolated_transcript(
                    transcript, "combine_arity/4"
                )

    def test_loads_twenty_isolated_pairs_and_parsed_resources(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        schedule = composition_performance.seeded_order_schedule(protocol)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject = root / "subject"
            control = root / "control"
            for block in range(1, 21):
                write_isolated_observation(subject, "direct/1", block, 105.0)
                write_isolated_observation(control, "direct/1", block, 100.0)

            pairs = composition_performance.load_isolated_runtime_pairs(
                subject, control, "direct/1", protocol
            )

        self.assertEqual(len(pairs), 20)
        self.assertEqual([row["order"] for row in pairs], schedule)
        self.assertEqual(pairs[0]["subject"], 105.0)
        self.assertEqual(pairs[0]["control_resources"]["cycles_elapsed"], 2000)

    def test_rejects_missing_per_process_resource_counters(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject = root / "subject"
            control = root / "control"
            for block in range(1, 21):
                write_isolated_observation(subject, "direct/1", block, 100.0)
                write_isolated_observation(control, "direct/1", block, 100.0)
            resource = (
                subject
                / "runtime/direct__1/block-01.resources.json"
            )
            document = json.loads(resource.read_text(encoding="utf-8"))
            del document["cycles_elapsed"]
            resource.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "resource counter set"):
                composition_performance.load_isolated_runtime_pairs(
                    subject, control, "direct/1", protocol
                )

    def test_loads_contemporaneous_clean_compile_pairs(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject = root / "subject"
            control = root / "control"
            for block in range(1, 21):
                write_compile_observation(subject, "direct", block, 6.1)
                write_compile_observation(control, "direct", block, 6.0)

            pairs = composition_performance.load_compile_pairs(
                subject, control, "direct", protocol
            )

        self.assertEqual(len(pairs), 20)
        self.assertEqual(pairs[0]["subject"], 6.1)
        self.assertEqual(pairs[0]["control"], 6.0)
        self.assertEqual(
            [row["order"] for row in pairs],
            composition_performance.seeded_order_schedule(protocol),
        )

    def test_evaluates_isolated_runtime_and_clean_compile_ucbs(self):
        budget_path = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = self._write_protocol_pair_fixture(
                root, protocol, "candidate-reference"
            )

            subject_report, control_report, comparison = (
                composition_performance.evaluate_protocol_paired(
                    budget_path,
                    protocol,
                    subject,
                    control,
                    plan,
                    {
                        "mode": "candidate-reference",
                        "subject_role": "candidate",
                        "control_role": "reference",
                        "subject_revision": "1" * 40,
                        "control_revision": composition_performance.REFERENCE_REVISION,
                        "os_version": "27.0",
                        "os_build": "26A428",
                    },
                )
            )

        self.assertEqual(subject_report["role"], "candidate")
        self.assertEqual(control_report["role"], "reference")
        self.assertEqual(len(comparison["rows"]), 14)
        self.assertTrue(comparison["passed"])
        self.assertTrue(
            all(row["upper_confidence_bound"] == 1 for row in comparison["rows"])
        )

    def test_evaluates_reference_aa_without_candidate_identity(self):
        budget_path = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = self._write_protocol_pair_fixture(
                root, protocol, "reference-reference-aa"
            )

            reports = composition_performance.evaluate_protocol_paired(
                budget_path,
                protocol,
                subject,
                control,
                plan,
                {
                    "mode": "reference-reference-aa",
                    "subject_role": "reference-b",
                    "control_role": "reference-a",
                    "subject_revision": composition_performance.REFERENCE_REVISION,
                    "control_revision": composition_performance.REFERENCE_REVISION,
                    "os_version": "27.0",
                    "os_build": "26A428",
                },
            )

        encoded = json.dumps(reports)
        self.assertNotIn("candidate", encoded)
        self.assertTrue(reports[2]["passed"])



class AbsoluteBudgetTests(unittest.TestCase):
    def test_current_absolute_uses_isolated_reference_arm_not_historical_timings(self):
        budget = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        protocol = composition_performance.load_protocol(REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            subject, control, plan = PairedComparisonTests()._write_protocol_pair_fixture(root, protocol, "reference-reference-aa")
            report, _, _ = composition_performance.evaluate_protocol_paired(
                budget, protocol, subject, control, plan,
                {"mode": "reference-reference-aa", "subject_role": "reference-b", "control_role": "reference-a", "subject_revision": composition_performance.REFERENCE_REVISION, "control_revision": composition_performance.REFERENCE_REVISION, "os_version": "27.0", "os_build": "26A428"},
            )
            raw = root / "raw"
            raw.mkdir()
            for path in (REPO_ROOT / "scripts/tests/fixtures/composition-performance/raw").iterdir():
                if path.name != "environment.txt" and not path.name.startswith(("runtime-divan-", "compile-")):
                    (raw / path.name).symlink_to(path)
            environment = (REPO_ROOT / "scripts/tests/fixtures/composition-performance/raw/environment.txt").read_text()
            current_environment = environment.replace("os_version=26.6.2", "os_version=27.0").replace("os_build=25G83", "os_build=26A428")
            (raw / "environment.txt").write_text(current_environment)
            absolute = composition_performance.evaluate_absolute(budget, raw, composition_performance.REFERENCE_REVISION, timing_report=report)
            timing = [row for row in absolute["rows"] if row["scenario"].startswith(("runtime/", "compile/"))]
            self.assertEqual(len(absolute["rows"]), 33)
            self.assertEqual(len(timing), 14)
            self.assertTrue(all(len(row["process_values"]) == 20 for row in timing))
            direct = next(row for row in timing if row["scenario"] == "runtime/direct/1")
            self.assertEqual(direct["measured_value"], 100)
            self.assertEqual(direct["raw_evidence"][0], str(subject / "runtime/direct__1/block-01.txt"))
            self.assertEqual(absolute["runner"], "mac15-9-m3-max-16c-48gb-macos27plus-ac-high")
            self.assertEqual(absolute["baseline_runner"], "mac15-9-m3-max-16c-48gb-macos26-ac-high")
            self.assertEqual(absolute["capture"]["os_version"], "27.0")
            self.assertEqual(absolute["capture"]["os_build"], "26A428")
            for invalid in (
                environment,
                current_environment.replace("os_version=27.0", "os_version=28.0"),
                current_environment.replace("os_build=26A428", "os_build=26A429"),
            ):
                with self.subTest(environment=invalid):
                    (raw / "environment.txt").write_text(invalid)
                    with self.assertRaisesRegex(ValueError, "macOS|os_version|os_build"):
                        composition_performance.evaluate_absolute(budget, raw, composition_performance.REFERENCE_REVISION, timing_report=report)
            (raw / "environment.txt").write_text(current_environment)
            with self.assertRaisesRegex(ValueError, "requires macOS 26"):
                composition_performance.evaluate_absolute(budget, raw, composition_performance.REFERENCE_REVISION)
            with self.assertRaisesRegex(ValueError, "timing report protocol"):
                composition_performance.evaluate_absolute(budget, raw, composition_performance.REFERENCE_REVISION, timing_report={**report, "protocol_sha256": "b5d0fddc975ca43883e2ee143f9794f5f21f1314a8cdd871ba4a49437470bf92"})
            report["revision"] = "1" * 40
            with self.assertRaisesRegex(ValueError, "timing report revision"):
                composition_performance.evaluate_absolute(budget, raw, composition_performance.REFERENCE_REVISION, timing_report=report)

    def test_corrected_binary_baseline_has_separate_receipt_backed_cohort(self):
        budget = composition_performance.validate_budget_document(
            REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        )
        provenance = budget["binary_baseline"]
        self.assertEqual(provenance["os_version"], "27.0")
        self.assertEqual(provenance["os_build"], "26A428")
        self.assertNotEqual(provenance["runner_cohort"], budget["runner_cohort"])
        raw = composition_performance._parse_binaries(
            REPO_ROOT / "docs/performance/composition" / provenance["binary_sizes"]["path"]
        )
        for row in budget["rows"]:
            if row["scenario"].startswith("binary/"):
                name = row["scenario"].split("/", 1)[1]
                self.assertEqual(row["baseline_value"], raw[name]["bytes"])
                self.assertEqual(row["absolute_budget"], (raw[name]["bytes"] * 11 + 9) // 10)
                self.assertEqual(row["runner_cohort"], provenance["runner_cohort"])

    def test_rejects_corrected_binary_provenance_and_budget_drift(self):
        root = REPO_ROOT / "docs/performance/composition"
        original = json.loads((root / "composition-budgets.json").read_text())
        for field, value in (("runner_cohort", composition_performance.BASELINE_RUNNER),
                             ("os_version", "26.0"), ("os_build", "wrong"),
                             ("measured_revision", "0" * 40)):
            with self.subTest(field=field):
                document = json.loads(json.dumps(original))
                document["binary_baseline"][field] = value
                with self.assertRaisesRegex(ValueError, "binary baseline provenance"):
                    composition_performance._validate_binary_baseline(document, root)
        for receipt in ("binary_sizes", "verification", "driver"):
            with self.subTest(receipt=receipt):
                document = json.loads(json.dumps(original))
                document["binary_baseline"][receipt]["sha256"] = "0" * 64
                with self.assertRaisesRegex(ValueError, "receipt hash mismatch"):
                    composition_performance._validate_binary_baseline(document, root)
        for field in ("baseline_value", "absolute_budget", "tolerance"):
            with self.subTest(field=field):
                document = json.loads(json.dumps(original))
                row = next(row for row in document["rows"] if row["scenario"].startswith("binary/"))
                row[field] += 1
                with self.assertRaisesRegex(ValueError, "binary baseline budget mismatch"):
                    composition_performance._validate_binary_baseline(document, root)

    def test_historical_unstripped_sizes_fail_corrected_binary_budgets(self):
        budget_path = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        raw_directory = REPO_ROOT / "scripts/tests/fixtures/composition-performance/raw"

        report = composition_performance.evaluate_absolute(
            budget_path, raw_directory, expected_revision=None
        )

        self.assertEqual(len(report["rows"]), 33)
        self.assertFalse(report["passed"])
        failures = [row for row in report["rows"] if not row["passed"]]
        self.assertEqual(len(failures), 6)
        self.assertTrue(all(row["scenario"].startswith("binary/") for row in failures))
        self.assertTrue(all(
            row["baseline_runner"] == composition_performance.BINARY_BASELINE_RUNNER
            for row in failures
        ))
        throughput = next(
            row
            for row in report["rows"]
            if row["scenario"] == "runtime/scoped_concurrent_senders_32"
            and row["measured_statistic"]
            == "median_throughput_across_processes"
        )
        self.assertEqual(throughput["direction"], "minimum")

    def test_rejects_missing_absolute_evidence(self):
        budget_path = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        with tempfile.TemporaryDirectory() as directory:
            raw_directory = pathlib.Path(directory)
            source = REPO_ROOT / "scripts/tests/fixtures/composition-performance/raw"
            for path in source.iterdir():
                (raw_directory / path.name).symlink_to(path)
            (raw_directory / "runtime-divan-01.txt").unlink()

            with self.assertRaisesRegex(ValueError, "transcript set is incomplete"):
                composition_performance.evaluate_absolute(
                    budget_path, raw_directory, expected_revision=None
                )

    def test_rejects_allocation_sample_and_logical_operation_drift(self):
        budget_path = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        source = REPO_ROOT / "scripts/tests/fixtures/composition-performance/raw"
        drifts = {
            "samples": "samples=99 logical_operations=100",
            "logical_operations": "samples=100 logical_operations=99",
        }
        for field, drift in drifts.items():
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                raw_directory = pathlib.Path(directory)
                for path in source.iterdir():
                    if path.name not in {"allocations.txt", "environment.txt"}:
                        (raw_directory / path.name).symlink_to(path)
                allocations = (source / "allocations.txt").read_text(
                    encoding="utf-8"
                )
                (raw_directory / "allocations.txt").write_text(
                    allocations.replace(
                        "scenario=effect_map_depth_1_outputs_1 samples=100 "
                        "logical_operations=100",
                        f"scenario=effect_map_depth_1_outputs_1 {drift}",
                        1,
                    ),
                    encoding="utf-8",
                )
                environment = (source / "environment.txt").read_text(
                    encoding="utf-8"
                )
                (raw_directory / "environment.txt").write_text(
                    environment
                    + "allocation_manifest_sha256="
                    + composition_performance.ALLOCATION_MANIFEST_SHA256
                    + "\n",
                    encoding="utf-8",
                )

                with self.assertRaisesRegex(ValueError, "allocation workload mismatch"):
                    composition_performance.evaluate_absolute(
                        budget_path, raw_directory, expected_revision=None
                    )


class BudgetValidationTests(unittest.TestCase):
    def test_rejects_wrong_reference_revision(self):
        source = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        document = json.loads(source.read_text(encoding="utf-8"))
        document["measured_revision"] = "0" * 40
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "budgets.json"
            path.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "reference revision"):
                composition_performance.validate_budget_document(
                    path,
                    "b9eb6e732d2b0cf6ce3cd7255a5958781587af6c",
                )

    def test_rejects_loosened_budget_with_hash_drift(self):
        source = REPO_ROOT / "docs/performance/composition/composition-budgets.json"
        document = json.loads(source.read_text(encoding="utf-8"))
        document["rows"][0]["absolute_budget"] *= 2
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "budgets.json"
            path.write_text(json.dumps(document), encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "budget file hash"):
                composition_performance.validate_budget_document(path)

    def test_rejects_changed_allocation_manifest_hash(self):
        source = REPO_ROOT / "scripts/composition-allocation-workloads.tsv"
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "workloads.tsv"
            path.write_text(
                source.read_text(encoding="utf-8").replace(
                    "effect_map_depth_1_outputs_1 100 100",
                    "effect_map_depth_1_outputs_1 99 99",
                    1,
                ),
                encoding="utf-8",
            )

            with self.assertRaisesRegex(ValueError, "manifest hash"):
                composition_performance.validate_allocation_manifest(path)


class ProtocolEvidenceTests(unittest.TestCase):
    def test_process_snapshot_requires_own_pid_and_unique_identities(self):
        for snapshot in ("", "200 1 rustc\n", "100 1 python\n100 1 cargo\n"):
            with self.subTest(snapshot=snapshot), self.assertRaises(ValueError):
                composition_performance.find_competing_processes(snapshot, 100)

    def test_rejects_nonfinite_pair_values(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        for invalid in (float("nan"), float("inf"), True, "1"):
            pairs = [dict(block=i, order=order, subject=1.0, control=1.0)
                     for i, order in enumerate(composition_performance.seeded_order_schedule(protocol), 1)]
            pairs[0]["subject"] = invalid
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                composition_performance.paired_ratio_statistics(pairs, "maximum", protocol)

    def test_rejects_other_language_compilers_outside_own_tree(self):
        snapshot = "100 1 python3\n101 100 swift-frontend\n200 1 /usr/bin/swift-frontend\n201 1 /usr/local/go/pkg/tool/darwin_arm64/compile\n202 1 /usr/bin/clang\n"
        self.assertEqual(
            [row["pid"] for row in composition_performance.find_competing_processes(snapshot, 100)],
            [200, 201, 202],
        )

    def test_quiescence_rejects_nonfinite_and_out_of_range_telemetry(self):
        protocol = composition_performance.load_protocol(REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json")
        for cpu, load in ((float("nan"), 0), (-1, 0), (0, -1), (0, float("nan"))):
            with self.subTest(cpu=cpu, load=load), self.assertRaises(ValueError):
                composition_performance.validate_quiescence_sample(
                    protocol, cpu_used_percent=cpu, load_average_1m=load,
                    thermal_text="Note: No thermal warning level has been recorded\nNote: No performance warning level has been recorded\nNote: No CPU power status has been recorded",
                    process_snapshot="100 1 python3", own_pid=100,
                )

    def test_parses_second_top_sample_and_load_average(self):
        top = """CPU usage: 90.0% user, 5.0% sys, 5.0% idle
CPU usage: 10.0% user, 4.0% sys, 86.0% idle
"""

        self.assertEqual(composition_performance.parse_cpu_used_percent(top), 14.0)
        self.assertEqual(
            composition_performance.parse_load_average_1m("{ 7.04 10.15 11.63 }"),
            7.04,
        )

    def test_rejects_missing_second_top_sample(self):
        with self.assertRaisesRegex(ValueError, "two CPU samples"):
            composition_performance.parse_cpu_used_percent(
                "CPU usage: 10.0% user, 4.0% sys, 86.0% idle\n"
            )

    def test_protocol_cli_emits_predeclared_plan(self):
        result = subprocess.run(
            [
                "python3",
                str(MODULE_PATH),
                "measurement-plan",
                "--protocol",
                str(REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"),
                "--mode",
                "candidate-reference",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["protocol_sha256"], composition_performance.PROTOCOL_SHA256)
        self.assertEqual(len(plan["runtime"]), 180)

    def test_time_cli_writes_machine_readable_counters(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            raw = root / "time.txt"
            output = root / "time.json"
            raw.write_text(
                """real 0.51
user 0.48
sys 0.02
1 maximum resident set size
0 page reclaims
0 page faults
0 swaps
0 block input operations
0 block output operations
0 voluntary context switches
0 involuntary context switches
1 instructions retired
1 cycles elapsed
1 peak memory footprint
""",
                encoding="utf-8",
            )

            result = subprocess.run(
                [
                    "python3",
                    str(MODULE_PATH),
                    "parse-time",
                    "--input",
                    str(raw),
                    "--output",
                    str(output),
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                json.loads(output.read_text(encoding="utf-8"))["cycles_elapsed"], 1
            )

    def test_parses_all_required_darwin_time_counters(self):
        transcript = """real 0.51
user 0.48
sys 0.02
          123456 maximum resident set size
               2 page reclaims
               0 page faults
               0 swaps
               3 block input operations
               4 block output operations
               5 voluntary context switches
               6 involuntary context switches
         7000000 instructions retired
         8000000 cycles elapsed
          654321 peak memory footprint
"""

        counters = composition_performance.parse_darwin_time(transcript)

        self.assertEqual(counters["real_seconds"], 0.51)
        self.assertEqual(counters["maximum_resident_set_bytes"], 123456)
        self.assertEqual(counters["voluntary_context_switches"], 5)
        self.assertEqual(counters["involuntary_context_switches"], 6)
        self.assertEqual(counters["instructions_retired"], 7_000_000)
        self.assertEqual(counters["cycles_elapsed"], 8_000_000)
        self.assertEqual(counters["peak_memory_footprint_bytes"], 654321)

    def test_rejects_incomplete_darwin_time_counters(self):
        with self.assertRaisesRegex(ValueError, "missing Darwin time counter"):
            composition_performance.parse_darwin_time("real 0.51\n")

    def test_process_snapshot_excludes_own_tree_and_rejects_competitor(self):
        snapshot = """ 100   50 /bin/bash
 101  100 /usr/bin/time
 102  101 /tmp/composition-owned
 200   50 /toolchain/bin/cargo
 201  200 /toolchain/bin/rustc
"""

        competitors = composition_performance.find_competing_processes(snapshot, 100)

        self.assertEqual([row["pid"] for row in competitors], [200, 201])

    def test_quiescence_uses_frozen_cpu_load_and_thermal_thresholds(self):
        protocol = composition_performance.load_protocol(
            REPO_ROOT / "docs/performance/composition/composition-protocol-v5.json"
        )
        thermal = """Note: No thermal warning level has been recorded
Note: No performance warning level has been recorded
Note: No CPU power status has been recorded
"""

        evidence = composition_performance.validate_quiescence_sample(
            protocol,
            cpu_used_percent=12.0,
            load_average_1m=5.0,
            thermal_text=thermal,
            process_snapshot=" 100 50 /bin/bash\n",
            own_pid=100,
        )

        self.assertTrue(evidence["passed"])
        with self.assertRaisesRegex(ValueError, "CPU threshold"):
            composition_performance.validate_quiescence_sample(
                protocol,
                cpu_used_percent=20.1,
                load_average_1m=5.0,
                thermal_text=thermal,
                process_snapshot=" 100 50 /bin/bash\n",
                own_pid=100,
            )
        with self.assertRaisesRegex(ValueError, "load threshold"):
            composition_performance.validate_quiescence_sample(
                protocol,
                cpu_used_percent=12.0,
                load_average_1m=6.1,
                thermal_text=thermal,
                process_snapshot=" 100 50 /bin/bash\n",
                own_pid=100,
            )
        with self.assertRaisesRegex(ValueError, "thermal warning-history"):
            composition_performance.validate_quiescence_sample(
                protocol,
                cpu_used_percent=1.0,
                load_average_1m=1.0,
                thermal_text="CPU_Speed_Limit = 80\n",
                process_snapshot=" 100 50 /bin/bash\n",
                own_pid=100,
            )



class QualificationShellTests(unittest.TestCase):
    def run_binary_capture(self, strip_body):
        source = (REPO_ROOT / "scripts/capture-composition-performance.sh").read_text()
        function = source.split("record_binary() {", 1)[1].split("\n}\n", 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            binary = root / "binary"
            binary.write_text("unstripped contents\n")
            binary.chmod(0o755)
            strip = root / "rust-objcopy"
            strip.write_text("#!/bin/sh\n" + strip_body)
            strip.chmod(0o755)
            result = subprocess.run(
                ["bash", "-c", "set -euo pipefail\nrecord_binary() {" + function
                 + '\n}\nrecord_binary fixture "$binary"'],
                env={**os.environ, "output_dir": directory, "objcopy_bin": str(strip),
                     "binary": str(binary)},
                capture_output=True, text=True, check=False,
            )
            receipt = root / "binary-sizes.txt"
            return result, receipt.read_text() if receipt.exists() else ""

    def test_binary_capture_rejects_strip_failure(self):
        result, receipt = self.run_binary_capture("exit 7\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(receipt, "")

    def test_binary_capture_measures_stripped_output(self):
        result, receipt = self.run_binary_capture('printf x > "$2"\n')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("bytes=1\n", receipt)

    def test_real_cargo_bloat_preflight_uses_cargo_subcommand(self):
        result = subprocess.run(
            [
                str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                "--check-prerequisites",
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo-bloat 0.12.1 is available", result.stdout)

    def test_preflight_rejects_every_cargo_profile_override(self):
        environment = dict(os.environ)
        environment["CARGO_PROFILE_FUTURE_OPT_LEVEL"] = "0"
        result = subprocess.run(
            [
                str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                "--check-prerequisites",
            ],
            cwd=REPO_ROOT,
            check=False,
            capture_output=True,
            text=True,
            env=environment,
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ambient benchmark overrides", result.stderr)

    def test_publishes_staged_evidence_with_darwin_mv(self):
        evidence_root = (
            REPO_ROOT
            / "target/qualification-evidence/0.1.0/composition"
        )
        evidence_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="shell-test-", dir=evidence_root) as directory:
            output = pathlib.Path(directory) / "published"
            tool_directory = pathlib.Path(directory) / "bin"
            tool_directory.mkdir()
            darwin_mv = tool_directory / "mv"
            darwin_mv.write_text(
                "#!/bin/sh\n"
                'if [ "${1:-}" = "--" ]; then\n'
                '  echo "mv: illegal option -- -" >&2\n'
                "  exit 64\n"
                "fi\n"
                'exec /bin/mv "$@"\n',
                encoding="utf-8",
            )
            darwin_mv.chmod(0o755)
            environment = dict(os.environ)
            environment["PATH"] = f"{tool_directory}:{environment['PATH']}"

            result = subprocess.run(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    "--check-publication",
                    str(output),
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
                env=environment,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                (output / "publication-smoke.txt").read_text(encoding="utf-8"),
                "complete\n",
            )
            self.assertFalse(pathlib.Path(f"{output}.incomplete").exists())

    def test_rejects_incomplete_staging_collision(self):
        evidence_root = (
            REPO_ROOT
            / "target/qualification-evidence/0.1.0/composition"
        )
        evidence_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="shell-test-", dir=evidence_root) as directory:
            output = pathlib.Path(directory) / "result"
            pathlib.Path(f"{output}.incomplete").mkdir()

            result = subprocess.run(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    str(output),
                    str(REPO_ROOT),
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("incomplete", result.stderr)

    def test_machine_lock_rejects_a_second_owner_atomically(self):
        with tempfile.TemporaryDirectory(prefix="rca-composition-lock-test.", dir="/tmp") as directory:
            lock = pathlib.Path(directory) / "whole-run.lock"
            first = subprocess.Popen(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    "--check-lock",
                    str(lock),
                    "2",
                ],
                cwd=REPO_ROOT,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            for _ in range(100):
                if lock.is_dir():
                    break
                import time

                time.sleep(0.01)
            second = subprocess.run(
                [
                    str(REPO_ROOT / "scripts/qualify-composition-performance.sh"),
                    "--check-lock",
                    str(lock),
                    "0",
                ],
                cwd=REPO_ROOT,
                check=False,
                capture_output=True,
                text=True,
            )
            first_stdout, first_stderr = first.communicate(timeout=5)

        self.assertEqual(first.returncode, 0, first_stderr)
        self.assertIn("lock acquired", first_stdout)
        self.assertNotEqual(second.returncode, 0)
        self.assertIn("whole-run lock is held", second.stderr)


if __name__ == "__main__":
    unittest.main()
