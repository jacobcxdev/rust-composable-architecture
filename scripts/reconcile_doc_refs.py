"""Request documentation repairs when the published index differs from the live Git refs."""

import argparse
import json
import os
import re
import subprocess
from urllib.request import Request, urlopen


OID = re.compile(r"[0-9a-f]{40}")
SITE = "https://docs.jacobcx.dev"


def live_refs(repository, default_branch):
    """Read all branch and tag heads in one request. Use the target of each annotated tag."""
    rows = subprocess.check_output(
        ["git", "ls-remote", "--heads", "--tags", "--", f"https://github.com/{repository}.git"],
        encoding="utf-8", timeout=60,
    )
    return parse_refs(rows, default_branch)


def parse_refs(rows, default_branch):
    """Reject a partial or malformed Git response before it can suggest a deletion."""
    found, peeled = {}, {}
    for line in rows.split("\n"):
        if not line:
            continue
        sha, ref = line.split("\t")
        if not OID.fullmatch(sha):
            raise ValueError("Invalid Git object ID in ref inventory")
        if ref.startswith("refs/heads/"):
            key, table = ("branch", ref.removeprefix("refs/heads/")), found
        elif ref.startswith("refs/tags/"):
            tag = ref.removeprefix("refs/tags/")
            key = ("tag", tag.removesuffix("^{}"))
            table = peeled if tag.endswith("^{}") else found
        else:
            raise ValueError("Unexpected namespace in ref inventory")
        if not key[1] or key in table:
            raise ValueError("Empty or duplicate Git ref in inventory")
        table[key] = sha

    if ("branch", default_branch) not in found or not peeled.keys() <= found.keys():
        raise ValueError("Git inventory lacks the default branch or an annotated tag")
    return found | peeled


def published_refs(repository_id):
    """Read the public inventory. A missing page or API error never means an empty index."""
    url = f"{SITE}/repo/{repository_id}/publications.json"
    request = Request(url, headers={"Accept": "application/json", "Cache-Control": "no-cache"})
    with urlopen(request, timeout=30) as response:
        data = json.load(response)

    if data["repositoryId"] != repository_id or not isinstance(data["publications"], list):
        raise ValueError("Documentation inventory does not match this repository")
    found = {}
    for row in data["publications"]:
        kind, name, sha = row["refType"], row["refName"], row["commitSha"]
        prefix = {"branch": "heads", "tag": "tags"}[kind]
        if (not isinstance(name, str) or not isinstance(sha, str) or not OID.fullmatch(sha)
                or (kind, name) in found
                or subprocess.run(["git", "check-ref-format", f"refs/{prefix}/{name}"]).returncode):
            raise ValueError("Invalid or duplicate ref in documentation inventory")
        found[kind, name] = sha
    return found


def reconcile(repository, repository_id, default_branch, dispatch):
    """Schedule only differences. Each worker rechecks the named ref before publication."""
    live = live_refs(repository, default_branch)
    published = published_refs(repository_id)
    pending = sorted(key for key in live.keys() | published.keys() if live.get(key) != published.get(key))
    print(f"{len(pending)} documentation repairs for {len(live)} current refs")
    for kind, name in pending:
        print(f"{'update' if (kind, name) in live else 'remove'} {kind} {json.dumps(name)}", flush=True)
        if dispatch:
            subprocess.run([
                "gh", "workflow", "run", "publish-docs.yml", "--repo", repository, "--ref", default_branch,
                "-f", f"kind={kind}", "-f", f"name={name}",
            ], check=True, timeout=30)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dispatch", action="store_true", help="request repairs instead of listing them")
    args = parser.parse_args()
    env = os.environ
    branch = env["DEFAULT_BRANCH"]
    if args.dispatch and (env.get("GITHUB_EVENT_NAME") not in {"schedule", "workflow_dispatch"}
                          or env.get("GITHUB_REF") != f"refs/heads/{branch}"
                          or env.get("GITHUB_ACTIONS") != "true"):
        parser.error("Dispatch requires a scheduled or manual workflow on the default branch.")
    reconcile(env["GITHUB_REPOSITORY"], int(env["GITHUB_REPOSITORY_ID"]), branch, args.dispatch)


if __name__ == "__main__":
    main()
