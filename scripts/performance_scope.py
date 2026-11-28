"""Select PR performance workloads. Selection is not a performance verdict."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess


ROOT = Path(__file__).resolve().parents[1]
FAMILIES = (
    "dispatch", "keyed-routing", "queue-pressure", "cancellation-shutdown",
    "cold-build", "headless-worker",
)

# Repository-only prose. The workspace README, benchmark inputs and
# publication rules are intentionally absent.
PROSE = {
    "AGENTS.md", "CONTRIBUTING.md", "SECURITY.md", ".github/pull_request_template.md",
    "docs/guides/authoring.md", "docs/guides/debounce-throttle.md",
    "docs/guides/dependencies-and-shared-state.md", "docs/guides/diagnostics.md",
    "docs/guides/external-clients.md", "docs/guides/owned-resources.md",
    "docs/guides/testing.md",
}

# Cargo discovers these as independent libtest executables, not as product or
# benchmark modules. Coverage still tests them. Do not extend this to their
# support code, allocation hosts, compiler fixtures or every file under tests/.
OBSERVATION_TESTS = {
    f"crates/rca-core/tests/observation_{suite}.rs"
    for suite in ("callbacks", "scope_lifecycle", "waker_lifecycle", "adapter_scopes")
}


def families_for(path):
    """Return affected consumer workloads, or every workload for unknown impact."""
    if path in PROSE or path in OBSERVATION_TESTS or re.fullmatch(r"scripts/tests/test_[a-z0-9_]+\.py", path):
        return ()
    if path.startswith("benches/compile/"):
        return ("cold-build",)
    if path in {"scripts/composition_cancellation_exposure.py", "crates/rca-core/src/effect/cancellation.rs"}:
        return ("cancellation-shutdown", "headless-worker")
    if path == "scripts/composition_compile_portfolio.py":
        return ("cold-build",)
    if re.fullmatch(r"crates/rca-core/src/(composition/keyed|store/removal)(/[^\n]+)?\.rs", path):
        return ("keyed-routing", "cancellation-shutdown", "headless-worker")
    if re.fullmatch(r"crates/rca-core/src/store/(mailbox(/[^\n]+)?|ingress)\.rs", path):
        return ("dispatch", "queue-pressure", "cancellation-shutdown", "headless-worker")
    # Actor, runtime, generated code, dependencies, toolchain, manifests and
    # unfamiliar paths can change more than the immediate caller.
    return FAMILIES


def scope_between_trees(repo, base, head):
    """Select by the committed input paths on both sides of the comparison."""
    raw = subprocess.check_output((
        "git", "-C", repo, "diff", "--no-ext-diff", "--no-textconv", "--no-renames",
        "--no-relative", "--ignore-submodules=none", "--name-only", "-z", base, head, "--",
    ))
    # --no-renames includes both sides, so moving a shipped file into a
    # documentation directory cannot hide its removal. Git paths can contain LF.
    paths = [os.fsdecode(path) for path in raw.split(b"\0") if path]
    selected = set()
    skipped = set()
    test_targets = set()
    broad = False
    for path in paths:
        affected = families_for(path)
        selected.update(affected)
        broad |= affected == FAMILIES
        if not affected:
            skipped.add(path.rsplit("/", 1)[-1])
        if path in OBSERVATION_TESTS:
            test_targets.add(Path(path).stem)

    def referenced(names, sources):
        if not names:
            return False
        patterns = [arg for name in sorted(names) for arg in ("-e", name)]
        references = subprocess.run((
            "git", "-C", repo, "grep", "--quiet", "--fixed-strings", *patterns, base, head,
            "--", *sources,
        ))
        if references.returncode not in (0, 1):
            references.check_returncode()
        return references.returncode == 0

    # A formerly standalone file can become a compiler input without a rename.
    # Cargo test targets can also be run by a measurement script by stem alone.
    # Check both trees. A false positive only expands the requested scope.
    rust_inputs = (":(top,glob)**/*.rs", ":(top,glob)**/Cargo.toml")
    harnesses = (*rust_inputs, ":(top)scripts", ":(top).github/workflows", ":(top)benches/performance",
                 ":(top,exclude)scripts/tests", ":(top,exclude)scripts/capture_coverage.py",
                 ":(top,exclude)scripts/performance_scope.py")
    if not broad and (referenced(skipped, rust_inputs) or referenced(test_targets, harnesses)):
        selected.update(FAMILIES)
        broad = True

    return {
        "base": base, "head": head, "changed_paths": len(paths),
        "scope": "broad" if broad else "focused" if selected else "none",
        "families": [name for name in FAMILIES if name in selected],
    }


def select(repo, base, head):
    """Compare two explicit Git commits, never the index or the working tree."""
    for commit in (base, head):
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", commit):
            raise ValueError("base and head must be full lowercase Git commit IDs")
        resolved = subprocess.check_output(("git", "-C", repo, "rev-parse", "--verify", f"{commit}^{{commit}}"))
        if resolved.decode().strip() != commit:
            raise ValueError("expected a commit, not an annotated tag")
    return scope_between_trees(repo, base, head)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT)
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    args = parser.parse_args()
    try:
        result = select(args.repo, args.base, args.head)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(2, f"Cannot select performance scope: {error}\n")
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    main()
