"""Select the source for a native comparison or first-release repeatability check."""

import argparse
from contextlib import contextmanager
from contextvars import ContextVar
from functools import lru_cache
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

from release_baseline import latest_baseline

ROOT = Path(__file__).resolve().parents[1]
_historical = ContextVar("rca_historical_reference", default=None)


def git(*args):
    return subprocess.check_output(["git", "--no-replace-objects", *args], cwd=ROOT)


@lru_cache(maxsize=4)
def signed_inputs(revision):
    """Reuse verified immutable Git objects during this process; never cache mutable HEAD."""
    git("verify-commit", revision)
    tree = git("rev-parse", f"{revision}^{{tree}}").decode().strip()
    return tree, hashlib.sha256(git("show", f"{revision}:Cargo.lock")).hexdigest()


def composition_reference(subject=None):
    """Select one public release identity for both native and composition workloads."""
    history = _historical.get()
    if history is not None:
        if subject is not None and subject != history["subject_revision"]:
            raise ValueError("historical replay cannot select another measured subject")
        return dict(history)
    tag = os.environ.get("RCA_QUALIFICATION_TAG")
    if subject is not None and not re.fullmatch(r"[0-9a-f]{40}", subject):
        raise ValueError("expected an exact candidate commit")
    if subject is not None and tag and subject != git("rev-parse", "HEAD").decode().strip():
        raise ValueError("a tagged release check requires the exact candidate checkout")
    baseline = latest_baseline(candidate_tag=tag)
    if baseline is None:
        head = git("rev-parse", "HEAD").decode().strip()
        # An unchanged candidate may be replayed from its documentation-only successor.
        if subject is not None and subject != head:
            git("merge-base", "--is-ancestor", subject, head)
        revision = subject or head
        tree, lock = signed_inputs(revision)
        return {"mode": "first-release-enrolment", "revision": revision, "tree": tree, "lock_sha256": lock}
    # A controller is not a measured subject. It can itself be the last release.
    if subject == baseline.revision:
        raise ValueError("a published release cannot be its own relative baseline")
    return {"mode": "release-comparison", "revision": baseline.revision, "tree": baseline.tree,
            "lock_sha256": baseline.lock_sha256}


def require_current_selection():
    """Historical replay cannot create new qualification evidence."""
    if _historical.get() is not None:
        raise ValueError("historical selection permits replay only")


@contextmanager
def historical_composition_reference(tag, expected_subject):
    """Pin one read-only replay to its signed published candidate and predecessor."""
    from release_baseline import historical_release_pair

    require_current_selection()
    if os.environ.get("RCA_QUALIFICATION_TAG"):
        raise ValueError("historical replay cannot override a current qualification tag")
    candidate, predecessor = historical_release_pair(tag)
    if candidate.tag != tag or candidate.revision != expected_subject:
        raise ValueError("recorded candidate does not match the signed published release")
    control = predecessor or candidate
    selected = {"mode": "release-comparison" if predecessor else "first-release-enrolment",
                "purpose": "historical-replay", "subject_tag": tag, "subject_revision": candidate.revision,
                "reference_tag": control.tag, "revision": control.revision,
                "tree": control.tree, "lock_sha256": control.lock_sha256}
    token = _historical.set(selected)
    try:
        yield dict(selected)
    finally:
        _historical.reset(token)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("revision")
    parser.add_argument("--replay-release", metavar="TAG")
    args = parser.parse_args()
    if args.replay_release is not None:
        if git("rev-parse", "HEAD").decode().strip() != args.revision:
            parser.error("native replay requires a checkout of the measured release")
        with historical_composition_reference(args.replay_release, args.revision) as selection:
            print(json.dumps(selection))
    else:
        print(json.dumps(composition_reference(args.revision)))
