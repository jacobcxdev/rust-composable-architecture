import importlib.util
import json
import pathlib
import tempfile
import unittest


MODULE = pathlib.Path(__file__).resolve().parents[1] / "common_outcome_allocations.py"
SPEC = importlib.util.spec_from_file_location("common_outcome_allocations", MODULE)
allocations = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(allocations)
PROTOCOL = MODULE.parents[1] / "docs/performance/composable-common-outcome-allocation-protocol-v1.json"


def ledger(engine="rca"):
    return dict(
        schema="composable-common-outcome-allocations-v1", engine=engine,
        instrument="dhat-0.3.3",
        work=dict(schema="composable-common-outcome-v1", engine=engine,
                  operations_expected=64, operations_attempted=64,
                  operations_completed=64, results=64, checkpoints=64,
                  checksum=6496, future_polls=64, future_drops=64, errors=0,
                  state_extracted=True, host_stopped=True,
                  completion="result-drop-checkpoint"),
        snapshots=dict(
            A=dict(allocated_blocks=3, allocated_bytes=24, live_blocks=1, live_bytes=8),
            E=dict(allocated_blocks=103, allocated_bytes=824, live_blocks=2, live_bytes=16)),
        lifetime=dict(operations=64, allocated_blocks=100, allocated_bytes=800))


class CommonOutcomeAllocationTests(unittest.TestCase):
    def test_accepts_each_engine_without_mutation(self):
        for engine in ("rca", "composable"):
            row = ledger(engine)
            before = json.dumps(row)
            self.assertIs(allocations.validate(row, engine), row)
            self.assertEqual(json.dumps(row), before)
            self.assertEqual(allocations.decode(before, engine), row)

    def test_rejects_incomplete_work_or_different_completion(self):
        for field, expected in ledger()["work"].items():
            for bad in (None, False, 0, 64.0, "wrong"):
                if type(bad) is type(expected) and bad == expected:
                    continue
                row = ledger()
                row["work"][field] = bad
                with self.subTest(field=field, bad=bad), self.assertRaises(ValueError):
                    allocations.validate(row, "rca")
        for completion in ("causal-finish", "result-acknowledgement"):
            row = ledger()
            row["work"]["completion"] = completion
            with self.assertRaises(ValueError):
                allocations.validate(row, "rca")

    def test_rejects_missing_extra_and_wrong_container_types(self):
        for path in ((), ("work",), ("snapshots",), ("snapshots", "A"),
                     ("snapshots", "E"), ("lifetime",)):
            for change in ("missing", "extra", "list"):
                row = ledger()
                target = row
                for key in path:
                    target = target[key]
                if change == "missing":
                    del target[next(iter(target))]
                elif change == "extra":
                    target["extra"] = 0
                elif path:
                    parent = row
                    for key in path[:-1]:
                        parent = parent[key]
                    parent[path[-1]] = []
                else:
                    row = []
                with self.subTest(path=path, change=change), self.assertRaises(ValueError):
                    allocations.validate(row, "rca")

    def test_rejects_invalid_counters_and_wrong_lifetime_deltas(self):
        for name in ("A", "E"):
            for field in ledger()["snapshots"][name]:
                for bad in (True, -1, 0.5, "1", None, 2**64):
                    row = ledger()
                    row["snapshots"][name][field] = bad
                    with self.subTest(name=name, field=field, bad=bad), self.assertRaises(ValueError):
                        allocations.validate(row, "rca")
        for field in ledger()["lifetime"]:
            for bad in (True, -1, 0.5, "1", None, 2**64, 99):
                row = ledger()
                row["lifetime"][field] = bad
                with self.subTest(field=field, bad=bad), self.assertRaises(ValueError):
                    allocations.validate(row, "rca")
        for suffix in ("blocks", "bytes"):
            row = ledger()
            row["snapshots"]["E"][f"allocated_{suffix}"] = 0
            with self.assertRaises(ValueError):
                allocations.validate(row, "rca")
            row = ledger()
            row["snapshots"]["E"][f"live_{suffix}"] = 1000
            with self.assertRaises(ValueError):
                allocations.validate(row, "rca")

    def test_accepts_zero_cost_and_falling_live_counts(self):
        row = ledger()
        for snapshot in row["snapshots"].values():
            for key in snapshot:
                snapshot[key] = 0
        row["lifetime"].update(allocated_blocks=0, allocated_bytes=0)
        self.assertIs(allocations.validate(row, "rca"), row)
        row = ledger()
        row["snapshots"]["E"].update(live_blocks=0, live_bytes=0)
        self.assertIs(allocations.validate(row, "rca"), row)

    def test_rejects_wrong_identity_and_duplicate_or_multiple_documents(self):
        for field in ("engine", "schema", "instrument"):
            row = ledger()
            row[field] = "wrong"
            with self.assertRaises(ValueError):
                allocations.validate(row, "rca")
        for engine in (None, True, [], "unknown", "composable", "rca-0.7"):
            with self.assertRaises(ValueError):
                allocations.validate(ledger(), engine)
        with self.assertRaises(ValueError):
            allocations.validate(ledger("rca-0.7"), "rca-0.7")
        text = json.dumps(ledger())
        for bad in ("", "not JSON", text + text,
                    text.replace('"checkpoints":', '"checkpoints":0,"checkpoints":'),
                    text.replace('"A":', '"A":{},"A":'),
                    text.replace('"allocated_bytes":', '"allocated_bytes":0,"allocated_bytes":')):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                allocations.decode(bad, "rca")


def observations():
    rows = []
    for cycle in (1, 2, 3):
        for order, role, engines in (
                ("forward", "a", ("composable", "rca")),
                ("reverse", "b", ("rca", "composable"))):
            for engine in engines:
                raw = ledger(engine)
                raw["snapshots"]["A"] = dict.fromkeys(raw["snapshots"]["A"], 0)
                blocks, size = {"composable": (100, 800),
                                "rca": (110, 880)}[engine]
                raw["snapshots"]["E"].update(allocated_blocks=blocks, allocated_bytes=size)
                raw["lifetime"].update(allocated_blocks=blocks, allocated_bytes=size)
                rows.append(dict(sequence=len(rows) + 1, cycle=cycle, order=order,
                                 role=role, engine=engine, raw=raw))
    return rows


class CommonOutcomeProtocolTests(unittest.TestCase):
    def test_fixed_plan_keeps_both_build_roles_and_all_cycles(self):
        protocol = allocations.load_protocol(PROTOCOL)
        plan = allocations.measurement_plan(protocol)
        self.assertEqual(len(plan), 12)
        self.assertEqual(plan[0], dict(sequence=1, cycle=1, order="forward", role="a", engine="composable"))
        self.assertEqual(plan[2], dict(sequence=3, cycle=1, order="reverse", role="b", engine="rca"))
        self.assertEqual(plan[-1], dict(sequence=12, cycle=3, order="reverse", role="b", engine="composable"))
        self.assertEqual([(p["engine"], p["role"]) for p in plan[:4]],
                         [("composable", "a"), ("rca", "a"),
                          ("rca", "b"), ("composable", "b")])

    def test_protocol_file_and_in_memory_mutations_are_rejected(self):
        original = PROTOCOL.read_bytes()
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "protocol.json"
            for content in (original + b"\n", original.replace(b'"cycles": 3', b'"cycles": 4')):
                path.write_bytes(content)
                with self.assertRaises(ValueError):
                    allocations.load_protocol(path)
        protocol = allocations.load_protocol(PROTOCOL)
        for field in protocol:
            altered = dict(protocol)
            altered[field] = None
            with self.subTest(field=field), self.assertRaises(ValueError):
                allocations.measurement_plan(altered)

    def test_analysis_keeps_absolute_costs_and_paired_differences_unqualified(self):
        rows = observations()
        before = json.dumps(rows)
        result = allocations.evaluate(rows, allocations.load_protocol(PROTOCOL))
        self.assertEqual(json.dumps(rows), before)
        self.assertFalse(result.get("qualified_comparison", True))
        self.assertFalse(result.get("provenance_qualified", True))
        self.assertEqual(result.get("observations"), 12)
        self.assertEqual(result["engines"]["rca"]["allocated_bytes"],
                         dict(values=[880] * 6, min=880, max=880))
        self.assertEqual(result["engines"]["rca"]["live_bytes"],
                         dict(values=[16] * 6, min=16, max=16))
        for row in result["comparisons"]["candidate-composable"]:
            self.assertEqual(row["difference"], dict(allocated_blocks=10, allocated_bytes=80,
                                                    live_blocks=0, live_bytes=0))
        self.assertEqual(len(result["comparisons"]["candidate-composable"]), 6)
        self.assertEqual(set(result["comparisons"]), {"candidate-composable"})

    def test_incomplete_reordered_or_nonzero_baseline_cohorts_are_rejected(self):
        protocol = allocations.load_protocol(PROTOCOL)
        bad_rows = [observations()[:-1], list(reversed(observations())), observations() + observations()[:1]]
        for field, bad in (("sequence", True), ("role", "b"), ("engine", "rca"), ("cycle", 2), ("extra", 0)):
            rows = observations()
            rows[0][field] = bad
            bad_rows.append(rows)
        rows = observations()
        rows[0]["raw"] = ledger("composable")
        bad_rows.append(rows)
        rows = observations()
        rows[0]["raw"]["work"]["errors"] = 1
        bad_rows.append(rows)
        for rows in bad_rows:
            with self.subTest(first=rows[0]), self.assertRaises(ValueError):
                allocations.evaluate(rows, protocol)

    def test_pairing_uses_same_cycle_and_build_role(self):
        rows = observations()
        for row in rows:
            blocks, size = row["sequence"] * 10, row["sequence"] * 80
            row["raw"]["snapshots"]["E"].update(allocated_blocks=blocks, allocated_bytes=size)
            row["raw"]["lifetime"].update(allocated_blocks=blocks, allocated_bytes=size)
        result = allocations.evaluate(rows, allocations.load_protocol(PROTOCOL))
        pairs = result["comparisons"]
        self.assertEqual([row["difference"]["allocated_bytes"] for row in pairs["candidate-composable"]],
                         [80, -80, 80, -80, 80, -80])
