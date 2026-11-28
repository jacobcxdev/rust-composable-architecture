"""Resolve the last published RCA runtime. A signed tag alone is not a publication."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from dataclasses import asdict, dataclass
from functools import lru_cache
import hashlib
from http.client import HTTPException, HTTPSConnection
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

from check_registry_archives import read_index
from check_release_tag import ROOT, check_version_sequence, github, oid, version
from package_registry import RCA_PACKAGES


PREVIEW_COMMIT = "925bcee4be19aa1f3f0360f33362a4a9cc2e7cea"


@dataclass(frozen=True)
class Release:
    tag: str
    tag_object: str
    revision: str
    tree: str
    lock_sha256: str


def git(*args):
    return subprocess.check_output(["git", "--no-replace-objects", *args], cwd=ROOT)


def git_id(expr):
    return oid(git("rev-parse", "--verify", expr).decode().strip())


def signed_release(tag, listed_commit):
    """Bind the local annotated tag and its target to GitHub's verified signature."""
    ref = github("git/ref/tags/" + tag)
    if ref["ref"] != "refs/tags/" + tag or ref["object"]["type"] != "tag":
        raise ValueError(f"{tag}: expected an annotated tag")
    tag_id = oid(ref["object"]["sha"])
    signed = github("git/tags/" + tag_id)
    signature = signed["verification"]
    commit = oid(signed["object"]["sha"])
    if (signed["sha"] != tag_id or signed["tag"] != tag or signed["object"]["type"] != "commit"
            or signature["verified"] is not True or signature["reason"] != "valid"
            or listed_commit != commit or git_id("refs/tags/" + tag) != tag_id
            or git_id(f"refs/tags/{tag}^{{commit}}") != commit):
        raise ValueError(f"{tag}: local and signed release identities differ")
    lock = git("show", f"{commit}:Cargo.lock")
    return Release(tag, tag_id, commit, git_id(commit + "^{tree}"), hashlib.sha256(lock).hexdigest())


def check_archive(name, number, checksum, revision):
    """Check the index checksum and source revision without unpacking files."""
    top = f"{name}-{number}"
    with closing(HTTPSConnection("static.crates.io", timeout=30)) as connection, tempfile.TemporaryFile() as archive:
        connection.request("GET", f"/crates/{name}/{top}.crate", headers={"User-Agent": "RCA-release-check"})
        with connection.getresponse() as reply:
            if reply.status != 200:
                raise ValueError(f"{top}: archive returned HTTP {reply.status}")
            shutil.copyfileobj(reply, archive)
        archive.seek(0)
        if hashlib.file_digest(archive, "sha256").hexdigest() != checksum:
            raise ValueError(f"{top}: package checksum differs from the registry")
        archive.seek(0)
        with tarfile.open(fileobj=archive, mode="r:gz") as files:
            entries = [m for m in files if m.name == top + "/.cargo_vcs_info.json"]
            if len(entries) != 1 or not entries[0].isfile() or entries[0].size > 65536:
                raise ValueError(f"{top}: missing or invalid Cargo source identity")
            with files.extractfile(entries[0]) as stream:
                metadata = json.load(stream)["git"]
            if metadata.get("sha1") != revision or metadata.get("dirty", False) is not False:
                raise ValueError(f"{top}: published source does not match the signed release")


def package_history(name):
    """Keep yanked versions in the history; a yanked latest version is not a baseline."""
    result = {}
    for row in read_index(name):
        number = version("v" + row["vers"])
        if (row["name"] != name or number in result or type(row["yanked"]) is not bool
                or not re.fullmatch(r"[a-f0-9]{64}", row["cksum"])):
            raise ValueError(f"{name}: invalid or duplicate registry record")
        result[number] = row
    return result


def latest_baseline(*, candidate_tag=None):
    """Read the canonical inventories once per command, outside the measurement."""
    return _resolve(candidate_tag)


def tagged_releases():
    preview = (0, 0, 1)
    tags = {}
    for row in github("tags"):
        key = version(row["name"])
        if key in tags:
            raise ValueError("duplicate release tag")
        tags[key] = (row["name"], oid(row["commit"]["sha"]))
    if len({commit for _, commit in tags.values()}) != len(tags):
        raise ValueError("release tags share a commit")
    if preview in tags:
        preview_release = signed_release(*tags[preview])
        if preview_release.revision != PREVIEW_COMMIT or git("ls-tree", preview_release.revision, "--", "rca-core"):
            raise ValueError("the signed snapshot preview differs from the original preview")
    full = {key for key in tags if key != preview}
    if full:
        check_version_sequence(tags)
    return tags, full


def checked_history(tags, full, pending=None):
    """An unavailable inventory or partial release must not look like an empty registry."""
    preview = (0, 0, 1)
    prior = full - {pending}
    names = sorted(RCA_PACKAGES)
    with ThreadPoolExecutor(max_workers=4) as workers:
        history = dict(zip(names, workers.map(package_history, names), strict=True))
    for name, entries in history.items():
        seen = set(entries)
        if preview in seen and preview in tags and name in {"rca", "rust-composable-architecture"}:
            seen.remove(preview)
        if (seen - {pending}) != prior:
            raise ValueError(f"{name}: release tags and published runtime versions differ")
    return history


def published_release(number, tags, history):
    release = signed_release(*tags[number])
    if not git("ls-tree", release.revision, "--", "rca-core/Cargo.toml"):
        raise ValueError("published tag has no runtime package")

    def check(name):
        row = history[name][number]
        if row["yanked"]:
            raise ValueError(f"{name}: selected published release is yanked")
        check_archive(name, row["vers"], row["cksum"], release.revision)

    with ThreadPoolExecutor(max_workers=4) as workers:
        list(workers.map(check, history))
    return release


@lru_cache(maxsize=None)
def _resolve(candidate_tag):
    tags, full = tagged_releases()
    pending = None
    if candidate_tag is not None:
        pending = version(candidate_tag)
        if not full or max(full) != pending or signed_release(*tags[pending]).revision != git_id("HEAD"):
            raise ValueError("candidate must be the newest signed full tag at the checked-out commit")
    prior = full - {pending}
    history = checked_history(tags, full, pending)
    return published_release(max(prior), tags, history) if prior else None


@lru_cache(maxsize=None)
def historical_release_pair(tag):
    """Authenticate the published release and its predecessor for replay only."""
    number = version(tag)
    tags, full = tagged_releases()
    if number not in full:
        raise ValueError(f"{tag}: historical replay requires a published full runtime release")
    history = checked_history(tags, full)
    subject = published_release(number, tags, history)
    git("verify-commit", subject.revision)
    prior = {key for key in full if key < number}
    if not prior:
        return subject, None
    reference = published_release(max(prior), tags, history)
    git("merge-base", "--is-ancestor", reference.revision, subject.revision)
    return subject, reference


def require_baseline():
    release = latest_baseline(candidate_tag=os.environ.get("RCA_QUALIFICATION_TAG"))
    if release is None:
        raise ValueError("no prior released runtime; a relative release comparison is not available")
    return release


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("field", choices=("record", "revision", "tree", "lock-sha256", "historical"))
    parser.add_argument("--candidate-tag", help="exclude this verified checkout's tag when selecting its predecessor")
    parser.add_argument("--release-tag", help="replay only this published release and its actual predecessor")
    args = parser.parse_args()
    if args.field == "historical":
        if not args.release_tag or args.candidate_tag:
            parser.error("historical replay requires --release-tag and takes no --candidate-tag")
        subject, reference = historical_release_pair(args.release_tag)
        print(json.dumps({"subject": asdict(subject), "reference": asdict(reference) if reference else None}, sort_keys=True))
        return
    if args.release_tag:
        parser.error("--release-tag is only valid for historical replay")
    release = latest_baseline(candidate_tag=args.candidate_tag)
    if args.field == "record":
        print(json.dumps(asdict(release) if release else None, sort_keys=True))
    elif release:
        print(getattr(release, args.field.replace("-", "_")))
    elif args.field == "revision":
        print("none")
    else:
        raise ValueError("no prior released runtime")


if __name__ == "__main__":
    try:
        main()
    except (HTTPException, KeyError, OSError, TypeError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        sys.exit(f"release baseline unavailable: {error}")
