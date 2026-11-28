"""Match documentation events to current branch and tag heads, not names in build output."""

import json
import os
from pathlib import Path
import re
import subprocess
from urllib.request import Request, urlopen


OID = re.compile(r"[0-9a-f]{40}")
KINDS = ("branch", "tag")
PUSH_WORKFLOW = ".github/workflows/docs-push.yml"


def read_refs(remote, name):
    """Read both namespaces in one advertisement. An unavailable remote is not an empty one."""
    branch, tag = f"refs/heads/{name}", f"refs/tags/{name}"
    if subprocess.run(["git", "check-ref-format", branch]).returncode:
        raise ValueError("Invalid documentation ref name")
    peeled = tag + "^{}"
    rows = subprocess.check_output(
        ["git", "ls-remote", "--quiet", "--", remote, branch, tag, peeled], encoding="utf-8"
    )
    found = {}
    for line in rows.split("\n"):
        if not line:
            continue
        sha, ref = line.split("\t")
        # ls-remote patterns also match the tail of a longer ref. Only exact names count.
        if ref in (branch, tag, peeled):
            if not OID.fullmatch(sha) or ref in found:
                raise ValueError("Invalid Git ref advertisement")
            found[ref] = sha
    result = {}
    if branch in found:
        result["branch"] = found[branch]
    if tag in found:
        result["tag"] = found.get(peeled, found[tag])
    return result


def check_commit(repository, oid):
    """On a manual tag repair, reject tags that point to trees or blobs instead of commits."""
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "rca-docs"}
    if token := os.environ.get("GH_TOKEN"):
        headers["Authorization"] = f"Bearer {token}"
    request = Request(f"https://api.github.com/repos/{repository}/git/commits/{oid}",
                      headers=headers)
    with urlopen(request, timeout=20) as response:
        if json.load(response)["sha"] != oid:
            raise ValueError("GitHub returned a different commit")


def resolve(event_name, event, repository, repository_id, lookup=read_refs, validate_commit=check_commit):
    """Return (operation, ref name, commit, kinds), or no work for a stale event."""
    source = event["repository"]
    if source["id"] != repository_id or source["full_name"] != repository:
        return None
    if event_name == "workflow_run":
        run = event["workflow_run"]
        if (event.get("action") != "completed" or run.get("status") != "completed"
                or run.get("event") != "push" or run.get("conclusion") != "success"
                or (run.get("head_repository") or {}).get("id") != repository_id
                or run.get("path") != PUSH_WORKFLOW):
            return None
        name, commit = run["head_branch"], run["head_sha"]
        if not isinstance(commit, str) or not OID.fullmatch(commit):
            raise ValueError("Expected a full GitHub workflow commit")
    elif event_name == "delete":
        name, kind = event["ref"], event["ref_type"]
    elif event_name == "workflow_dispatch":
        if event["ref"] != "refs/heads/" + source["default_branch"]:
            return None
        name, kind = event["inputs"]["name"], event["inputs"]["kind"]
    else:
        return None

    if not isinstance(name, str) or not name or (event_name != "workflow_run" and kind not in KINDS):
        raise ValueError("Expected a branch or tag name")
    live = lookup(f"https://github.com/{repository}.git", name)
    if event_name == "workflow_run":
        # GitHub gives head_branch for both push kinds. Same-name refs can share a build
        # only if their current commits both match the run.
        kinds = [kind for kind in KINDS if live.get(kind) == commit]
        return ("upsert", name, commit, kinds) if kinds else None
    if kind not in live:
        return "delete", name, "", [kind]
    if event_name == "delete":
        return None  # Someone recreated this ref after GitHub queued its deletion.
    if kind == "tag":
        validate_commit(repository, live[kind])
    return "upsert", name, live[kind], [kind]


def main():
    env = os.environ
    result = resolve(env["GITHUB_EVENT_NAME"], json.loads(Path(env["GITHUB_EVENT_PATH"]).read_bytes()),
                     env["GITHUB_REPOSITORY"], int(env["GITHUB_REPOSITORY_ID"]))
    operation, name, commit, kinds = result or ("", "", "", [])
    with Path(env["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
        print(f"active={'true' if result else 'false'}\noperation={operation}\nname={name}\ncommit={commit}",
              file=output)
        print("kinds=" + json.dumps(kinds, separators=(",", ":")), file=output)


if __name__ == "__main__":
    main()
