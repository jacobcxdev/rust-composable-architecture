"""Verify a stable RCA release tag without changing GitHub or the registry."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

from package_registry import RCA_PACKAGES


ROOT = Path(__file__).resolve().parents[1]
REPO = "jacobcxdev/rust-composable-architecture"
TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def version(tag):
    """Read only canonical, stable release tags."""
    match = TAG_PATTERN.fullmatch(tag)
    if not match:
        raise ValueError(f"not a stable RCA release tag: {tag!r}")
    return tuple(map(int, match.groups()))


def check_version_sequence(versions):
    """Accept full releases from v0.1.0, with an optional v0.0.1 preview."""
    ordered = sorted(versions)
    if ordered and ordered[0] == (0, 0, 1):
        ordered = ordered[1:]
    if not ordered or ordered[0] != (0, 1, 0):
        raise ValueError("the first RCA release must be v0.1.0")
    for (major, minor, patch), current in zip(ordered, ordered[1:]):
        if current not in ((major, minor, patch + 1), (major, minor + 1, 0), (major + 1, 0, 0)):
            raise ValueError("release tags skip a version")


def github(endpoint):
    """Read from the canonical repository. Include every page of inventory."""
    args = ["gh", "api", "--hostname", "github.com", "--method", "GET"]
    paged = endpoint in ("tags", "releases")
    if paged:
        args += ["--paginate", "--slurp"]
    path = f"repos/{REPO}/{endpoint}" + ("?per_page=100" if paged else "")
    data = json.loads(subprocess.check_output([*args, path]))
    if paged:
        if not isinstance(data, list) or not all(isinstance(page, list) for page in data):
            raise ValueError(f"invalid GitHub {endpoint} inventory")
        return [item for page in data for item in page]
    return data


def oid(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value) or value == "0" * 40:
        raise ValueError("GitHub object identity must be a full nonzero SHA")
    return value


def verify_tag(tag, pushed, checkout, get):
    """Check the signed tag, release sequence, checked-out source and live main."""
    wanted = version(tag)
    ref = get(f"git/ref/tags/{tag}")
    if ref["ref"] != f"refs/tags/{tag}" or ref["object"]["type"] != "tag":
        raise ValueError("an annotated tag is required")
    tag_oid = oid(ref["object"]["sha"])
    signed = get(f"git/tags/{tag_oid}")
    verified = signed["verification"]
    if (signed["sha"] != tag_oid or signed["tag"] != tag or signed["object"]["type"] != "commit"
            or verified["verified"] is not True or verified["reason"] != "valid"):
        raise ValueError("GitHub must verify a signed tag that points directly to a commit")
    commit = oid(signed["object"]["sha"])
    if oid(checkout) != commit or oid(pushed) not in (tag_oid, commit):
        raise ValueError("checkout or push does not match the verified tag")

    tags = {}
    for item in get("tags"):
        release = version(item["name"])
        if release in tags:
            raise ValueError("duplicate release tag")
        tags[release] = oid(item["commit"]["sha"])
    if len(tags) != len(set(tags.values())):
        raise ValueError("release tags share a commit")
    if tags.get(wanted) != commit or wanted != max(tags):
        raise ValueError("the requested release must be the latest tag and identify the same commit")
    check_version_sequence(tags)

    seen = set()
    for item in get("releases"):
        release = version(item["tag_name"])
        if item["draft"] is not False or item["prerelease"] is not False or release not in tags or release in seen:
            raise ValueError("resolve conflicting, draft or prerelease GitHub releases before publication")
        seen.add(release)

    main = get("git/ref/heads/main")
    if main["ref"] != "refs/heads/main" or main["object"]["type"] != "commit" or main["object"]["sha"] != commit:
        raise ValueError("the verified commit is not the current tip of main")
    return tag[1:]


def check_packages(expected, root):
    """Check that the crates and the skill plugin all ship at the tag's version."""
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    inherited = workspace["package"]["version"]
    if inherited != expected:
        raise ValueError(f"workspace version must be {expected}")
    plugin = json.loads((root / "plugins" / "rca" / "plugin.json").read_text())
    if not isinstance(plugin, dict) or plugin.get("version") != expected:
        raise ValueError(f"skill plugin version must be {expected}")
    found = set()
    for member in workspace["members"]:
        package = tomllib.loads((root / member / "Cargo.toml").read_text())["package"]
        if package.get("publish") is False:
            continue
        name = package["name"]
        declared = package["version"]
        actual = inherited if declared == {"workspace": True} else declared
        if actual != expected or name in found:
            raise ValueError(f"duplicate package or wrong version: {name}")
        found.add(name)
    if found != RCA_PACKAGES:
        raise ValueError("release must contain exactly the expected RCA packages")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--sha", required=True, help="full GitHub push or tag SHA")
    args = parser.parse_args()
    if os.getenv("GITHUB_ACTIONS") == "true" and (
            os.getenv("GITHUB_REPOSITORY") != REPO or os.getenv("GITHUB_REF") != f"refs/tags/{args.tag}"):
        raise ValueError("workflow must run on the release tag in the canonical repository")
    checkout = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=all"], cwd=ROOT):
        raise ValueError("release checkout must be clean")
    version(args.tag)
    check_packages(args.tag[1:], ROOT)
    print(verify_tag(args.tag, args.sha, checkout, github))


if __name__ == "__main__":
    try:
        main()
    except (KeyError, OSError, TypeError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"release preflight failed: {error}")
