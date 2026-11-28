"""Select documentation CI only when the unchanged build inputs have a green run."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess


ROOT_DOCS = {"README.md", "AGENTS.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md",
             ".github/pull_request_template.md"}
PLUGIN_MANIFESTS = {"plugins/rca/plugin.json", ".agents/plugins/marketplace.json"}


def git(*args):
    return subprocess.check_output(["git", *args])


def matches(base, head, query, paths):
    # Only git-grep's explicit "no matches" result can narrow CI.
    return subprocess.run(["git", "grep", "-q", *query, base, head, "--", *paths]).returncode != 1


def mode_between(base, head):
    """Read both paths of every move. Reject non-regular inputs, including symlinks."""
    fields = git("diff", "--raw", "--no-renames", "--no-ext-diff", "--ignore-submodules=none",
                 "--no-relative", "-z", base, head, "--").split(b"\0")[:-1]
    paths = set()
    for header, raw in zip(fields[::2], fields[1::2], strict=True):
        old, new, *_ = header.removeprefix(b":").split()
        if {old, new} - {b"100644", b"000000"}:
            return "full"
        path = os.fsdecode(raw)
        if (path not in ROOT_DOCS | PLUGIN_MANIFESTS and not
                re.fullmatch(r"(?:docs|plugins/rca/skills)/(?:[\w-]+/)*[\w-]+\.md", path, re.ASCII)):
            return "full"
        paths.add(path)
    if not paths:
        return "full"

    # A Cargo build script can traverse a directory without naming a guide.
    # RCA has none. If one appears, qualify its inputs before narrowing CI.
    tree = git("ls-tree", "-r", "--name-only", "-z", head)
    custom_build = """(^|[,{[:space:]])["']?build["']?[[:space:]]*=[[:space:]]*["']"""
    if (any(path.rsplit(b"/", 1)[-1] == b"build.rs" for path in tree.split(b"\0"))
            or matches(base, head, ["-E", "-e", custom_build], [":(glob)**/Cargo.toml"])):
        return "full"
    # A guide can become include_str! data without moving. Check both trees.
    # Cargo's README metadata is covered separately by the actual package tests.
    names = {Path(path).name for path in paths}
    for inputs, search in [((":(glob)**/*.rs",), names),
                           ((":(glob)**/Cargo.toml",), names - {"README.md"})]:
        if search:
            terms = [part for name in sorted(search) for part in ("-e", name)]
            if matches(base, head, ["-F", *terms], inputs):
                return "full"
    return "readme" if "README.md" in paths else "docs"


def green(workflow, repo, commit, event, branch, title=None):
    # Run titles capture the base when CI starts. The API's pull_requests object
    # reflects the PR TODAY, not necessarily the merge that an older run tested.
    path = f"repos/{repo}/actions/workflows/{workflow}/runs"
    result = subprocess.check_output(["gh", "api", "-X", "GET", path, "-f", f"head_sha={commit}",
                                     "-f", f"event={event}", "-f", f"branch={branch}", "-f", "per_page=1"])
    runs = json.loads(result)["workflow_runs"]
    return bool(runs and runs[0]["head_sha"] == commit and runs[0]["conclusion"] == "success"
                and runs[0]["head_repository"]["full_name"] == repo
                and (title is None or runs[0]["display_title"] == title))


def select(workflow, repo, kind, ref, event, checked):
    """Use the tested merge tree, not a truncated file list or only the last commit."""
    def ancestor(base, head):
        return (isinstance(base, str) and re.fullmatch("[0-9a-f]{40}", base) and
                subprocess.run(["git", "merge-base", "--is-ancestor", base, head]).returncode == 0)

    previous = event.get("before", "")
    if kind == "push" and ref.startswith("refs/heads/") and ancestor(previous, checked):
        mode = mode_between(previous, checked)
        if mode != "full" and green(workflow, repo, previous, "push", ref.removeprefix("refs/heads/")):
            return mode, previous
    elif kind == "pull_request":
        pr = event["pull_request"]
        base, head = pr["base"]["sha"], pr["head"]["sha"]
        if git("show", "-s", "--format=%P", checked).decode().split() != [base, head]:
            return "full", None
        mode = mode_between(base, checked)
        if mode != "full" and green(workflow, repo, base, "push", pr["base"]["ref"]):
            return mode, base
        # A documentation edit on an open code PR can reuse its own last
        # successful run only on the same base and with a known source tree.
        if (event.get("action") == "synchronize" and pr["head"]["repo"]["full_name"] == repo
                and ancestor(previous, head) and ancestor(base, previous)):
            mode = mode_between(previous, checked)
            title = f"PR {event['number']} against {base}"
            if mode != "full" and green(workflow, repo, previous, kind, pr["head"]["ref"], title):
                return mode, previous
    return "full", None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workflow", choices=["ci.yml", "performance-feedback.yml"])
    args = parser.parse_args()
    env = os.environ
    try:
        mode, baseline = select(args.workflow, env["GITHUB_REPOSITORY"], env["GITHUB_EVENT_NAME"],
                                env["GITHUB_REF"], json.loads(Path(env["GITHUB_EVENT_PATH"]).read_bytes()),
                                env["GITHUB_SHA"])
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"Cannot prove documentation-only scope: {error}. Selecting full CI.")
        mode, baseline = "full", None
    with open(env["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        print(f"mode={mode}", file=output)
    with open(env["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
        print(f"Selected **{mode}** checks." + (f" Unchanged Rust inputs passed at `{baseline}`."
                                               if baseline else ""), file=summary)


if __name__ == "__main__":
    main()
