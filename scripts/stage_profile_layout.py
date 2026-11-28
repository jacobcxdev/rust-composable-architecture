"""Keep one profiling fixture when a comparison checkout has a different crate directory."""

import argparse
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
PROFILE = Path("tests/performance-profile")
INPUTS = ("Cargo.toml", "src/allocation_main.rs", "src/profile_items.rs",
          "src/bin/controlled_cost.rs", "tests/support/mod.rs")


def stage(baseline, *, check=False):
    """Change only crate paths in pristine copies of the candidate fixture's inputs."""
    baseline = baseline.resolve(strict=True)
    roots = [part for part in (Path("crates"), Path()) if (baseline / part / "rca-core/Cargo.toml").is_file()]
    if len(roots) != 1:
        raise ValueError("expected exactly one RCA crate directory in the comparison checkout")
    for relative in INPUTS:
        path = baseline / PROFILE / relative
        if path.resolve(strict=True) != path:
            raise ValueError(f"redirected profiling input: {relative}")
        candidate = (ROOT / PROFILE / relative).read_bytes()
        expected = candidate if roots[0] == Path("crates") else candidate.replace(b"../crates/rca-", b"../rca-")
        if path.read_bytes() != (expected if check else candidate):
            raise ValueError(f"profiling input differs from the candidate: {relative}")
        if not check and expected != candidate:
            path.write_bytes(expected)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("--check", action="store_true", help="verify the staged inputs without changing them")
    args = parser.parse_args()
    stage(args.baseline, check=args.check)
