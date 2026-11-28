"""Compare archives from `cargo package` with the public crates.io index.

Supply a directory with all RCA .crate files for the requested version.
Use a full checkout with current release tags. The next version need not have a tag yet.
Every package must have exactly its prior releases. Only the two public package names
may have the v0.0.1 preview. Past yanks count as history. An existing copy must not be yanked and must match
the local archive byte for byte by SHA-256. Only a 404 means the registry has no entry.
The command prints missing package names after all checks pass. It never uploads files.
"""

import argparse
from contextlib import closing
import hashlib
from http.client import HTTPException, HTTPSConnection
import json
from pathlib import Path
import subprocess
import sys

from check_release_tag import check_version_sequence, version as parse_tag
from package_registry import RCA_PACKAGES, index_path


def read_index(name):
    """Read all versions for one crate. A redirect or service error stops the check."""
    path = "/" + index_path(Path(), name).as_posix()
    with closing(HTTPSConnection("index.crates.io", timeout=20)) as connection:
        connection.request("GET", path, headers={"User-Agent": "RCA-release-check"})
        with connection.getresponse() as response:
            if response.status == 404:
                return []
            if response.status != 200:
                raise ValueError(f"{name}: crates.io returned HTTP {response.status}")
            records = [json.loads(line) for line in response]
    if not records:
        raise ValueError(f"{name}: crates.io returned an empty index")
    return records


def prior_versions(version, tags):
    """Accept a tagged release or the next stable version in the same stream."""
    current = parse_tag("v" + version)
    versions = {parse_tag(tag): tag[1:] for tag in tags}
    expected = versions.keys() | {current}
    if max(expected) != current:
        raise ValueError("the requested version is older than a release tag")
    check_version_sequence(expected)
    return {number for parts, number in versions.items() if parts < current}


def check_archives(version, directory, tags, lookup, require_complete):
    """Return the missing names only if every local archive and index entry passes."""
    prior = prior_versions(version, tags)
    expected = {f"{name}-{version}.crate" for name in RCA_PACKAGES}
    actual = {path.name for path in directory.glob("*.crate")}
    if actual - expected:
        raise ValueError(f"unexpected archive files: {', '.join(sorted(actual - expected))}")
    if expected - actual:
        raise FileNotFoundError(f"missing archive files: {', '.join(sorted(expected - actual))}")

    missing = []
    for name in sorted(RCA_PACKAGES):
        archive = directory / f"{name}-{version}.crate"
        with archive.open("rb") as stream:
            checksum = hashlib.file_digest(stream, "sha256").hexdigest()
        seen = set()
        for entry in lookup(name):
            if not isinstance(entry, dict) or entry.get("name") != name:
                raise ValueError(f"{name}: invalid crate name in the registry index")
            number = entry.get("vers")
            if not isinstance(number, str) or number in seen or not isinstance(entry.get("yanked"), bool):
                raise ValueError(f"{name}: invalid or duplicate registry entry")
            seen.add(number)
            if number == version:
                if entry["yanked"]:
                    raise ValueError(f"{name} {version}: the published version is yanked")
                if entry.get("cksum") != checksum:
                    raise ValueError(f"{name} {version}: the archive differs from the registry checksum")
        history = seen - {version}
        expected_history = prior if name in {"rca", "rust-composable-architecture"} else prior - {"0.0.1"}
        if history != expected_history:
            raise ValueError(
                f"{name}: registry history {sorted(history)} differs from release tags {sorted(expected_history)}"
            )
        if version not in seen:
            missing.append(name)
    if require_complete and missing:
        raise ValueError(f"missing from crates.io: {', '.join(missing)}")
    return missing


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--archives", required=True, type=Path, help="directory with all RCA .crate files")
    parser.add_argument("--require-complete", action="store_true", help="reject any missing version")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    shallow = subprocess.check_output(["git", "rev-parse", "--is-shallow-repository"], cwd=root, text=True)
    if shallow.strip() != "false":
        raise ValueError("fetch the full release tag history before this check")
    tags = subprocess.check_output(["git", "tag", "--list"], cwd=root, text=True).splitlines()
    missing = check_archives(args.version, args.archives, tags, read_index, args.require_complete)
    for name in missing:
        print(name)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, HTTPException, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
