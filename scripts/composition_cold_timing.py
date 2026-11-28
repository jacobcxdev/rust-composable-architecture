"""Experimental decisions over real Windows timing prefixes; not a release qualifier.

The default bounds enclose the final twenty-block statistic. The optional looks
use confidence bounds for the relative medians, not the realised original
ranks. All methods retain the original absolute bound and all measured inputs.
"""

import math
import statistics

import composition_performance as evidence


def _json_bounds(lower, upper):
    """Use null for an unbounded supremum so every receipt is valid JSON."""
    return [lower, upper if math.isfinite(upper) else None]


def _decision(states):
    if not states or any(state not in ("pass", "fail", "inconclusive", "continue") for state in states):
        raise ValueError("missing or invalid timing gate decision")
    return next((state for state in ("fail", "inconclusive", "continue") if state in states), "pass")


def _relative(pairs, direction, protocol, *, single_look=False, two_look=False):
    orders = evidence._paired_ratios(pairs, direction, protocol, len(pairs), two_look=two_look)
    lower, upper = [], []
    for ratios in orders.values():
        if any(not math.isfinite(ratio) or ratio <= 0 for ratio in ratios):
            raise ValueError("relative timing ratios must be finite and positive")
        if two_look:
            # Final statistic per order: min(max(first seven), rank 11 of
            # first twelve). Its extremes set all missing ratios to 0 or infinity.
            first = max(ratios[:7], default=0.)
            observed = sorted(ratios)
            lower.append(min(first, observed[-2] if len(observed) >= 2 else 0.))
            upper.append(min(first if len(observed) >= 7 else math.inf,
                             observed[10] if len(observed) >= 11 else math.inf))
            continue
        if single_look:
            # One preselected one-sided limit for each median. Do not search
            # later observations for a more favourable set of seven.
            upper.append(max(ratios[:7]) if len(ratios) >= 7 else math.inf)
            continue
        observed = sorted(ratios)
        # In ten completed values, rank nine is second-largest. Assigning every
        # unseen ratio below or above every observation gives the two extremes.
        lower.append(observed[-2] if len(observed) >= 2 else 0.)
        upper.append(observed[8] if len(observed) >= 9 else math.inf)
    hi = math.sqrt(math.prod(upper))
    if single_look:
        if all(map(math.isfinite, upper)) and not math.isfinite(hi):
            raise ValueError("single-look relative timing statistic overflowed")
        limit = protocol["regression_limit"]
        state = "continue" if not math.isfinite(hi) else "pass" if hi <= limit else "inconclusive"
        return {"decision": state, "single_look_upper": hi if math.isfinite(hi) else None,
                "regression_limit": limit}
    lo = math.sqrt(math.prod(lower))
    if not math.isfinite(lo) or (all(map(math.isfinite, upper)) and not math.isfinite(hi)):
        raise ValueError("relative timing statistic overflowed")
    limit = protocol["regression_limit"]
    if two_look:
        return {"decision": "inconclusive" if lo > limit else "pass" if hi <= limit else "continue",
                "two_look_upper_bounds": _json_bounds(lo, hi), "regression_limit": limit}
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


def timing_gate_bounds(aa_pairs, candidate_pairs, direction, protocol, *, single_look=False, two_look=False):
    """Score two real prefixes with the selected relative rule and the original absolute gate."""
    confidence = protocol["confidence"]
    if single_look and two_look:
        raise ValueError("select either the single-look or the two-look method")
    if (protocol.get("method_version") != evidence.WINDOWS_V6_METHOD
            or confidence.get("observations_per_stratum") != 10
            or confidence.get("upper_order_statistic_rank") != 9
            or protocol.get("regression_limit") != evidence.REGRESSION_LIMIT
            or direction not in ("minimum", "maximum")
            or not 1 <= len(aa_pairs) == len(candidate_pairs) <= (24 if two_look else 20)):
        raise ValueError("timing bounds require the Windows v6 rules and matched mode prefixes")

    aa = _relative(aa_pairs, direction, protocol, single_look=single_look, two_look=two_look)
    candidate = _relative(candidate_pairs, direction, protocol, single_look=single_look, two_look=two_look)
    ref_lo, ref_hi = _median_bounds(aa_pairs[:20])
    lo, hi = _median_bounds(candidate_pairs[:20])
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
    decision = _decision([aa["decision"], candidate["decision"], absolute["decision"]])
    return {"decision": decision,
            "relative_aa": aa, "relative_candidate": candidate, "absolute_candidate": absolute,
            "absolute_aa": {"decision": "pass", "same_reference_subject": True}}


def timing_decisions(directory, protocol, budget_path, counts_by_workload, seals, *, single_look=False, two_look=False):
    """Return each workload's joint decision; only 'continue' requests another real block.

    The collector starts all thirteen workloads and advances both comparison modes
    in the frozen order. Retired workload data remains present and verified.
    """
    from composition_timing_inputs import load_timing_prefixes

    if single_look and two_look:
        raise ValueError("select either the single-look or the two-look method")
    prefixes = load_timing_prefixes(directory, protocol, budget_path, counts_by_workload, seals, two_look=two_look)
    result = {}
    for workload, rows in prefixes.items():
        gates = []
        previous = []
        for row, aa, candidate in rows:
            direction = evidence._direction(row)
            proof = timing_gate_bounds(aa, candidate, direction, protocol, single_look=single_look, two_look=two_look)
            gates.append({"scenario": row["scenario"], "measured_statistic": row["measured_statistic"],
                          **proof})
            if (single_look or two_look) and len(aa) > 1:
                previous.append(timing_gate_bounds(aa[:-1], candidate[:-1], direction, protocol,
                                                   single_look=single_look, two_look=two_look)["decision"])
        decision = _decision([gate["decision"] for gate in gates])
        if single_look or two_look:
            # Once no completion can pass, the collector stops. Never admit extra
            # samples to turn an inconclusive result into an absolute failure.
            label = "two-look" if two_look else "single-look"
            if previous and _decision(previous) != "continue":
                raise ValueError(f"{label} workload continued after its decision: {workload}")
            if decision != "pass" and counts_by_workload[workload] < max(counts_by_workload.values()):
                raise ValueError(f"{label} capture continued after a terminal workload: {workload}")
        if decision == "continue" and counts_by_workload[workload] < max(counts_by_workload.values()):
            raise ValueError(f"workload retired before its verdict was fixed: {workload}")
        result[workload] = {"decision": decision, "blocks": counts_by_workload[workload], "gates": gates}
    return result
