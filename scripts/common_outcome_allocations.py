"""Admit whole-lifetime common-outcome allocation records, without qualification."""

import hashlib
import json


PROTOCOL_SHA256 = "1614616f7079911fcaf574bc5b3802d8422b34b547fea3085224a10c939a77b3"
_DOCUMENT_SHA256 = "5e03dd3547273bdf831926c9f10ad70b560b143263ce9e370eac9700d832eb9d"


def load_protocol(path):
    """Require the frozen bytes before interpreting the measurement contract."""
    content = path.read_bytes()
    if hashlib.sha256(content).hexdigest() != PROTOCOL_SHA256:
        raise ValueError("common-outcome protocol bytes differ")
    return json.loads(content)


def measurement_plan(protocol):
    """Expand the frozen order without selecting or retrying measurements."""
    document = json.dumps(protocol, sort_keys=True, separators=(",", ":")).encode()
    if hashlib.sha256(document).hexdigest() != _DOCUMENT_SHA256:
        raise ValueError("common-outcome protocol differs")
    plan = []
    for cycle in range(1, protocol["cycles"] + 1):
        for order in protocol["orders"]:
            engines = protocol["engine_order"]
            if order["order"] == "reverse":
                engines = list(reversed(engines))
            for engine in engines:
                plan.append(dict(sequence=len(plan) + 1, cycle=cycle, **order, engine=engine))
    return plan


def evaluate(rows, protocol):
    """Retain whole-lifetime observations without authenticating their provenance."""
    plan = measurement_plan(protocol)
    if type(rows) is not list or len(rows) != len(plan):
        raise ValueError("common-outcome observations are incomplete")
    for row, expected in zip(rows, plan):
        _fields(row, expected.keys() | {"raw"})
        _matches(row, expected)
        validate(row["raw"], expected["engine"])
        if any(row["raw"]["snapshots"]["A"].values()):
            raise ValueError("common-outcome initial snapshot is not zero")
    engines = {}
    for engine in protocol["engine_order"]:
        samples = [row["raw"]["snapshots"]["E"] for row in rows if row["engine"] == engine]
        metrics = {}
        for metric in protocol["metrics"]:
            values = [sample[metric] for sample in samples]
            metrics[metric] = dict(values=values, min=min(values), max=max(values))
        engines[engine] = dict(samples=len(samples), **metrics)
    by_identity = {(row["cycle"], row["order"], row["engine"]): row["raw"]["snapshots"]["E"]
                   for row in rows}
    comparisons = {}
    for name, baseline in (("candidate-composable", "composable"), ("candidate-historical", "rca-0.7")):
        pairs = []
        for row in rows:
            if row["engine"] != "rca":
                continue
            candidate = row["raw"]["snapshots"]["E"]
            control = by_identity[row["cycle"], row["order"], baseline]
            pairs.append(dict(cycle=row["cycle"], order=row["order"], role=row["role"],
                              difference={metric: candidate[metric] - control[metric]
                                          for metric in protocol["metrics"]}))
        comparisons[name] = pairs
    return dict(schema="composable-common-outcome-allocation-analysis-v1",
                protocol_sha256=PROTOCOL_SHA256, observations=len(rows),
                operations_per_process=protocol["operations"], completion=protocol["completion"],
                engines=engines, comparisons=comparisons,
                provenance_qualified=False, qualified_comparison=False,
                interpretation="Whole-lifetime costs include fixture coordination. No per-operation, "
                               "steady-state, statistical, allocator-quiescence or superiority claim.")


def validate(value, expected_engine):
    """Check exact work, snapshots and deltas without granting performance parity."""
    if type(expected_engine) is not str or expected_engine not in ("composable", "rca-0.7", "rca"):
        raise ValueError("common-outcome engine is invalid")
    _fields(value, {"schema", "engine", "instrument", "work", "snapshots", "lifetime"})
    _matches(value, dict(schema="composable-common-outcome-allocations-v1",
                         engine=expected_engine, instrument="dhat-0.3.3"))
    required_work = dict(schema="composable-common-outcome-v1", engine=expected_engine,
                         operations_expected=64, operations_attempted=64,
                         operations_completed=64, results=64, checkpoints=64,
                         checksum=6496, future_polls=64, future_drops=64, errors=0,
                         state_extracted=True, host_stopped=True,
                         completion="result-drop-checkpoint")
    _fields(value["work"], required_work.keys())
    _matches(value["work"], required_work)
    snapshots = value["snapshots"]
    _fields(snapshots, {"A", "E"})
    previous = None
    for name in ("A", "E"):
        snapshot = snapshots[name]
        _fields(snapshot, {"allocated_blocks", "allocated_bytes", "live_blocks", "live_bytes"})
        if any(type(count) is not int or not 0 <= count < 2**64 for count in snapshot.values()):
            raise ValueError("common-outcome allocation counter is invalid")
        for suffix in ("blocks", "bytes"):
            total = snapshot[f"allocated_{suffix}"]
            if snapshot[f"live_{suffix}"] > total:
                raise ValueError("common-outcome live count exceeds total")
            if previous is not None and total < previous[f"allocated_{suffix}"]:
                raise ValueError("common-outcome cumulative count decreased")
        previous = snapshot
    required_lifetime = dict(
        operations=64,
        **{field: snapshots["E"][field] - snapshots["A"][field]
           for field in ("allocated_blocks", "allocated_bytes")})
    _fields(value["lifetime"], required_lifetime.keys())
    _matches(value["lifetime"], required_lifetime)
    return value


def _fields(value, required):
    if type(value) is not dict or value.keys() != required:
        raise ValueError("common-outcome fields are invalid")


def _matches(value, required):
    for field, expected in required.items():
        if type(value[field]) is not type(expected) or value[field] != expected:
            raise ValueError(f"common-outcome mismatch: {field}")


def decode(text, expected_engine):
    """Reject duplicate keys and multiple documents before admitting a record."""
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("common-outcome JSON has duplicate fields")
            result[key] = value
        return result

    return validate(json.loads(text, object_pairs_hook=unique_object), expected_engine)
