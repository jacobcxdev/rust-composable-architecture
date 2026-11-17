"""Experimental decisions over real Windows timing prefixes; not a release qualifier.

Every bound encloses the *final twenty-block statistic*, not a confidence bound
estimated from fewer samples. No unseen observation is recorded as measured.
"""

import math
import statistics

import composition_performance as evidence


def _json_bounds(lower, upper):
    """Use null for an unbounded supremum so every receipt is valid JSON."""
    return [lower, upper if math.isfinite(upper) else None]


def _decision(states):
    if "fail" in states:
        return "fail"
    return "continue" if "continue" in states else "pass"


def _relative(pairs, direction, protocol):
    orders = evidence._paired_ratios(pairs, direction, protocol, len(pairs))
    lower, upper = [], []
    for ratios in orders.values():
        if any(not math.isfinite(ratio) or ratio <= 0 for ratio in ratios):
            raise ValueError("relative timing ratios must be finite and positive")
        observed = sorted(ratios)
        # In ten completed values, rank nine is second-largest. Assigning every
        # unseen ratio below or above every observation gives the two extremes.
        lower.append(observed[-2] if len(observed) >= 2 else 0.)
        upper.append(observed[8] if len(observed) >= 9 else math.inf)
    lo = math.sqrt(math.prod(lower))
    hi = math.sqrt(math.prod(upper))
    if not math.isfinite(lo) or (all(map(math.isfinite, upper)) and not math.isfinite(hi)):
        raise ValueError("relative timing statistic overflowed")
    limit = protocol["regression_limit"]
    return {"decision": "fail" if lo > limit else "pass" if hi <= limit else "continue",
            "final_upper_bound": _json_bounds(lo, hi), "regression_limit": limit}


def _median_bounds(pairs):
    values = sorted(row["subject"] for row in pairs)
    # The frozen absolute statistic is the arithmetic midpoint of ranks 10 and
    # 11. Each unseen value may shift an observed rank by at most one; index
    # those two ranks directly, without adding observations to either arm.
    missing = 20 - len(values)
    lo = statistics.median(values[i-missing] if i >= missing else 0. for i in (9, 10))
    hi = statistics.median(values[i] if i < len(values) else math.inf for i in (9, 10))
    if not math.isfinite(lo) or (len(values) >= 11 and not math.isfinite(hi)):
        raise ValueError("absolute timing median overflowed")
    return lo, hi


def timing_gate_bounds(aa_pairs, candidate_pairs, direction, protocol):
    """Decide one metric for all valid completions of both real, sealed mode prefixes."""
    confidence = protocol["confidence"]
    if (protocol.get("method_version") != evidence.WINDOWS_V6_METHOD
            or confidence.get("observations_per_stratum") != 10
            or confidence.get("upper_order_statistic_rank") != 9
            or protocol.get("regression_limit") != evidence.REGRESSION_LIMIT
            or direction not in ("minimum", "maximum")
            or not 1 <= len(aa_pairs) == len(candidate_pairs) <= 20):
        raise ValueError("timing bounds require the Windows v6 rules and matched mode prefixes")

    aa = _relative(aa_pairs, direction, protocol)
    candidate = _relative(candidate_pairs, direction, protocol)
    ref_lo, ref_hi = _median_bounds(aa_pairs)
    lo, hi = _median_bounds(candidate_pairs)
    # Match seal_reference and _passes operation-for-operation, including which
    # operand is multiplied/divided. Ratio comparisons can round differently.
    factor = protocol["regression_limit"]
    if direction == "maximum":
        budget_lo, budget_hi = ref_lo * factor, ref_hi * factor
        passed, failed = hi <= budget_lo, lo > budget_hi
    else:
        budget_lo, budget_hi = ref_lo / factor, ref_hi / factor
        passed, failed = lo >= budget_hi, hi < budget_lo
    if not math.isfinite(budget_lo) or (math.isfinite(ref_hi) and not math.isfinite(budget_hi)):
        raise ValueError("absolute timing budget overflowed")
    absolute = {"decision": "pass" if passed else "fail" if failed else "continue",
                "direction": direction, "subject_median": _json_bounds(lo, hi),
                "reference_median": _json_bounds(ref_lo, ref_hi),
                "absolute_budget": _json_bounds(budget_lo, budget_hi)}
    # The A/A absolute report uses the very same reference-b median on both
    # sides, not an independent draw from its envelope. Its paired gate remains.
    return {"decision": _decision([aa["decision"], candidate["decision"], absolute["decision"]]),
            "relative_aa": aa, "relative_candidate": candidate, "absolute_candidate": absolute,
            "absolute_aa": {"decision": "pass", "same_reference_subject": True}}


def timing_decisions(directory, protocol, budget_path, counts_by_workload, seals):
    """Return each workload's joint decision; only 'continue' requests another real block.

    The collector starts all thirteen workloads and advances both comparison modes
    in the frozen order. Retired workload data remains present and verified.
    """
    from composition_timing_inputs import load_timing_prefixes

    prefixes = load_timing_prefixes(directory, protocol, budget_path, counts_by_workload, seals)
    result = {}
    for workload, rows in prefixes.items():
        gates = []
        for row, aa, candidate in rows:
            proof = timing_gate_bounds(aa, candidate, evidence._direction(row), protocol)
            gates.append({"scenario": row["scenario"], "measured_statistic": row["measured_statistic"],
                          **proof})
        decision = _decision([gate["decision"] for gate in gates])
        if decision == "continue" and counts_by_workload[workload] < max(counts_by_workload.values()):
            raise ValueError(f"workload retired before its verdict was fixed: {workload}")
        result[workload] = {"decision": decision, "blocks": counts_by_workload[workload], "gates": gates}
    return result
