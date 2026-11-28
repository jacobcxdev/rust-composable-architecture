"""Synthetic parser inputs. Nothing in this module is a benchmark result."""

import hashlib


def divan_output(process=1):
    """Build one complete Divan table, including grouped and ungrouped rows."""
    lines = ["composition  fastest │ slowest │ median │ mean │ samples │ iters"]
    groups = {"direct": (1,), "nonmatching_scope": (16,), "combine_arity": (4, 12),
              "keyed_matching_route": (100000,), "effect_map_depth": (16,),
              "descendant_cancellation": (10000,), "matching_scope": (16,)}
    lines.append(f"├─ scoped_concurrent_senders_32  1 µs │ 20 µs │ {process} µs │ 5 µs │ 50 │ 50")
    for group, cases in groups.items():
        lines.append(f"├─ {group}  │")
        for case in cases:
            lines.append(f"│  ├─ {case}  1 ns │ 20 ns │ {process} ns │ 5 ns │ 50 │ 50")
    lines.extend(f"├─ diagnostic_{index}  1 ns │ 20 ns │ {process} ns │ 5 ns │ 50 │ 50"
                 for index in range(153 - 1 - sum(map(len, groups.values()))))
    return "\n".join(lines) + "\n"


def allocation_output(workloads):
    """Keep the real operation counts but give each scenario invented measurements."""
    return "".join(
        f"scenario={name} samples={row['samples']} logical_operations={row['logical_operations']} "
        f"allocations={index} allocated_bytes={index * 64} retained_bytes=0 peak_live_bytes=64\n"
        for index, (name, row) in enumerate(workloads.items(), 1)
    )


def zero_output(name):
    return (f"test {name} ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
            "3 filtered out; finished in 0.01s\n")


def write_product_inputs(raw, allocation_scenarios, binary_bytes):
    """Write only the product records the absolute-budget parser reads."""
    (raw / "allocations.txt").write_text(allocation_output(allocation_scenarios), encoding="utf-8")
    for label, case in (
        ("zero-direct-scope", "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up"),
        ("zero-combine", "composition::combine::tests::combine_all_none_widths_do_not_allocate"),
    ):
        (raw / f"{label}.txt").write_text(zero_output(case), encoding="utf-8")
    (raw / "binary-sizes.txt").write_text("".join(
        f"binary={name}\nbytes={size}\nsha256={hashlib.sha256(name.encode()).hexdigest()}\n"
        "identity=Mach-O 64-bit executable arm64\n" for name, size in binary_bytes.items()
    ), encoding="utf-8")
