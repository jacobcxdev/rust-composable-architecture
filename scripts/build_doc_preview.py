"""Build rustdoc from a source checkout without giving it access to the runner."""

import argparse
import os
from pathlib import Path
import secrets
import stat
import subprocess
import sys
import tempfile


IMAGE = "rust:1.100.0-bookworm@sha256:75d26c23c24cc6ea1cdabf0714a6bff8fd7bf3248716043fd7ae86dac7f41b94"
CHECKER = Path(__file__).resolve().with_name("prepare_public_docs.py")


def regular_tree(root):
    """Reject links and special files before any process reads the output."""
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("The documentation root must be a directory.")
    pending = [root]
    while pending:
        for entry in pending.pop().iterdir():
            info = entry.lstat()
            if stat.S_ISDIR(info.st_mode):
                pending.append(entry)
            elif not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
                raise ValueError("The documentation contains a link or a special file.")


def build(source, output):
    """Export a fresh, checked documentation directory, or leave no publication."""
    source = source.resolve(strict=True)
    output = output.parent.resolve(strict=True) / output.name
    if os.path.lexists(output) or output.is_relative_to(source):
        raise ValueError("Use a new output directory outside the source checkout.")
    if any(":" in str(path) for path in (source, output.parent, CHECKER)):
        raise ValueError("Docker mount paths cannot contain a colon.")

    with tempfile.TemporaryDirectory(prefix="rca-rustdoc-", dir=output.parent) as scratch:
        checker = "/rca-control/scripts/prepare_public_docs.py"
        runner = [
            "docker", "run", "--rm", "--init", "--read-only",
            "--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit=384",
            "--memory=4g", "--memory-swap=4g",
            "--user", f"{os.getuid()}:{os.getgid()}", "--tmpfs", "/tmp:rw,nosuid,nodev,mode=1777",
            "--volume", f"{source}:/rca-source:ro", "--volume", f"{scratch}:/rca-target:rw",
            "--volume", f"{CHECKER}:{checker}:ro",
            "--workdir", "/rca-source", "--env", "HOME=/tmp/rca-home",
            "--env", "CARGO_HOME=/tmp/rca-cargo", "--env", "CARGO_TARGET_DIR=/rca-target",
            "--env", "CARGO_BUILD_JOBS=2", "--env", "RUSTUP_TOOLCHAIN=1.100.0", "--env", "RUSTDOCFLAGS=-D warnings",
        ]
        # Source code and filenames can write to the log. Never interpret their output as runner commands.
        token = secrets.token_hex(32)
        print(f"::stop-commands::{token}", flush=True)
        try:
            command = [IMAGE, "python3", "-I", checker, "/rca-target/doc", "--source-root", "/rca-source"]
            subprocess.run([*runner, *command, "--compile-only"], check=True)
            docs = Path(scratch) / "doc"
            regular_tree(docs)
            # Use the controller's checker in a fresh container, never the one in the source checkout.
            subprocess.run([*runner, "--network", "none", *command], check=True)
            regular_tree(docs)
            docs.rename(output)
        finally:
            print(f"::{token}::", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("This build requires a Linux runner. On macOS, use the Cargo memory watchdog.")
    build(args.source, args.output)
