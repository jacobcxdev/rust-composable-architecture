#!/usr/bin/env python3
"""Check every tracked or non-ignored Rust source with the pinned rustfmt."""

from collections import defaultdict
from functools import cache
import os
from pathlib import Path
import subprocess
import sys
import tomllib


# These fragments sit outside their consuming packages' manifest directories.
SHARED_2021 = (
    "tests/composable-counter/",
    "tests/composable-keyed-policy/",
    "tests/composable-observation/",
    "tests/composable-stream/",
    "tests/composable-timers/",
)


@cache
def manifest(path):
    """Read a manifest once during this check."""
    with path.open("rb") as source:
        return tomllib.load(source)


def source_edition(root, relative):
    """Use the owning manifest, with explicit ownership for shared fragments."""
    for directory in (root / relative).parents:
        path = directory / "Cargo.toml"
        if path.is_file():
            package = manifest(path).get("package")
            if package is not None:
                edition = package.get("edition", "2015")
                if edition == {"workspace": True}:
                    for ancestor in (directory, *directory.parents):
                        workspace_path = ancestor / "Cargo.toml"
                        if workspace_path.is_file() and "workspace" in manifest(workspace_path):
                            return manifest(workspace_path)["workspace"]["package"]["edition"]
                        if ancestor == root:
                            break
                    raise ValueError(f"No workspace edition found for {relative}")
                return edition
        if directory == root:
            break

    if relative.startswith(SHARED_2021):
        return "2021"
    return manifest(root / "Cargo.toml")["workspace"]["package"]["edition"]


def main():
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    raw = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", "*.rs"],
        cwd=root,
    )
    names = sorted({os.fsdecode(name) for name in raw.split(b"\0") if name})
    if not names:
        raise ValueError("No Rust source files found")

    groups = defaultdict(list)
    for name in names:
        path = root / name
        for part in (path, *path.parents):
            if part == root:
                break
            if part.is_symlink():
                raise ValueError(f"Rust source uses a symlink: {name}")
        # Git still lists tracked deletions until they are staged.
        if not path.exists():
            continue
        edition = source_edition(root, name)
        if edition not in ("2015", "2018", "2021", "2024"):
            raise ValueError(f"Unsupported Rust edition for {name}: {edition!r}")
        groups[edition].append(str(path))

    count = sum(map(len, groups.values()))
    if not count:
        raise ValueError("No Rust source files found")
    channel = manifest(root / "rust-toolchain.toml")["toolchain"]["channel"]
    failed = False
    for edition, paths in sorted(groups.items()):
        for start in range(0, len(paths), 100):
            result = subprocess.run(
                ["rustup", "run", channel, "rustfmt", "--check", "--edition", edition,
                 "--config-path", str(root / "rustfmt.toml"),
                 "--config", "skip_children=true", *paths[start:start + 100]],
                cwd=root,
                check=False,
            )
            failed |= result.returncode != 0
    print(f"Checked {count} Rust files across {len(groups)} edition groups", flush=True)
    return int(failed)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        print(f"Rust formatting check failed: {error}", file=sys.stderr)
        sys.exit(1)
