"""Select documentation CI only when the unchanged build inputs have a green run."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

if __package__:
    from . import ci_scope_inputs
else:
    import ci_scope_inputs


PLUGIN_MANIFESTS = {"plugins/rca/plugin.json", ".agents/plugins/marketplace.json"}
ROOT_PACKAGE_DOCS = {"README.md", "LICENSE-APACHE.md", "LICENSE-MIT.md"}


def git(*args):
    return subprocess.check_output(["git", *args])


# A rustdoc link made from the repository URL does not read its destination.
# Only exempt link definitions. Other doc attributes can contain runnable tests.
_URL_PART = rb'(?:"[\w./#%-]+"|env!\(\s*"(?:RCA_DOCS_REVISION|CARGO_PKG_VERSION)"\s*\))'
_DOC_LINK = (rb'"\\n\[[\w -]+\]: "\s*,\s*env!\(\s*"CARGO_PKG_REPOSITORY"\s*\)'
             rb'(?:\s*,\s*' + _URL_PART + rb')+')
_DOC_TEXT = re.compile(rb'#!?\[\s*(?:cfg_attr\(\s*(?:\w+|not\(\w+\))\s*,\s*)?'
                       rb'doc\s*=\s*concat!\(\s*' + _DOC_LINK + rb'(?:\s*,\s*' + _DOC_LINK
                       + rb')*\s*,?\s*\)\s*\)?\s*\]')


def rust_mentions(head, paths):
    """Keep possible runtime file reads; a prose citation does not read bytes."""
    names = {os.fsencode(Path(path).name).lower() for path in paths}
    terms = [arg for name in sorted(names) for arg in ("-e", name)]
    hits = subprocess.run(["git", "grep", "-a", "-i", "-l", "-z", "-F", *terms, head, "--", ":(glob)**/*.rs"],
                          stdout=subprocess.PIPE)
    if hits.returncode == 1:
        return False
    hits.check_returncode()
    for _, source in ci_scope_inputs.read_blobs(hits.stdout.rstrip(b"\0").split(b"\0")):
        example = False
        for line in _DOC_TEXT.sub(b"", source).splitlines():
            line = line.lstrip()
            if line.startswith((b"///", b"//!")):
                body = line[3:].removeprefix(b" ")
                # Keep fenced and indented doctests. Once a block contains a
                # potential example, inspect the rest; no Markdown parser needed.
                example |= (b"```" in body or b"~~~" in body or body.startswith((b"    ", b"\t")))
                if not example:
                    continue
            else:
                example = False
                if line.startswith(b"//"):
                    continue
            if any(name in line.lower() for name in names):
                return True
    return False


def cargo_markdown_mode(head, paths, tree):
    """Separate package metadata from possible compiler inputs in the unchanged manifests."""
    changed = {path.casefold() for path in paths}
    names = {Path(path).name for path in changed}
    mode = "docs"
    specs = [os.fsencode(head) + b":" + entry.partition(b"\t")[2] for entry in tree
             if entry.endswith((b"\tCargo.toml", b"/Cargo.toml"))]
    for spec, contents in ci_scope_inputs.read_blobs(specs):
        file = Path(os.fsdecode(spec.partition(b":")[2]))
        manifest = tomllib.loads(contents.decode())
        for package in (manifest.get("package", {}), manifest.get("project", {}),
                        manifest.get("workspace", {}).get("package", {})):
            if package.get("build"):
                return "full"
            # Inherited paths start at the workspace manifest, not the member.
            for field in ("readme", "license-file", "license_file"):
                value = package.pop(field, None)
                if isinstance(value, str):
                    path = os.path.normpath(file.parent / value.replace("\\", "/")).casefold()
                    if path in changed:
                        mode = "readme"
                    elif (os.path.isabs(path) or ":" in path or path.startswith("../")) and Path(path).name in names:
                        return "full"
        # A remaining mention can name a target source or a tool input, even README.md.
        remaining = str(manifest).casefold()
        if any(name in remaining for name in names):
            return "full"
    return mode


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
        if not path.endswith(".md") and path not in PLUGIN_MANIFESTS:
            return "full"
        paths.add(path)
    if not paths:
        return "full"
    return markdown_mode(head, paths)


def markdown_mode(head, paths):
    """Choose checks after the diff proves only regular documentation inputs changed."""
    # Runtime tools can discover a fixture without naming it in source.
    attrs = git("check-attr", "-z", "--source=" + head, "rca-ci-input", "--", *sorted(paths))
    if any(value != b"unspecified" for value in attrs.split(b"\0")[2::3]):
        return "full"

    # A Cargo build script can traverse a directory without naming a guide.
    # RCA has none. If one appears, qualify its inputs before narrowing CI.
    tree = git("ls-tree", "-r", "-z", head).split(b"\0")
    if any(entry.startswith(b"120000 ") or entry.endswith((b"/build.rs", b"\tbuild.rs")) for entry in tree):
        return "full"
    # Only Markdown or skill metadata changed. Rust files and manifests match
    # on both sides, so inspect the committed source tree once.
    if ci_scope_inputs.has_markdown_include(head, paths) or rust_mentions(head, paths):
        return "full"
    cargo = cargo_markdown_mode(head, paths, tree)
    if cargo == "full":
        return cargo
    packaged = any(path in ROOT_PACKAGE_DOCS or path.startswith(("crates/", "dependencies/")) for path in paths)
    return "readme" if packaged else cargo


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
