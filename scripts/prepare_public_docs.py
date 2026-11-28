"""Prepare rustdoc's publication tree and reject paths from the build machine."""

import argparse
import html
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]
SOURCE_DESCRIPTION = re.compile(rb'(<meta name="description" content="Source of the Rust file `)[^"]*(`\.">)')
LANDING = (
    '<!doctype html><html lang="en"><head><meta charset="utf-8">'
    '<meta http-equiv="refresh" content="0; url=rca/index.html"><title>Rust Composable Architecture</title>'
    '</head><body><a href="rca/index.html">Open the API documentation</a></body></html>\n'
)


def private_paths(output, source_root):
    """Match literal and escaped build paths in published files, not in build logs."""
    paths = {Path.home(), Path.cwd(), ROOT, source_root, output, Path(tempfile.gettempdir())}
    for key in ("CARGO_HOME", "RUSTUP_HOME", "CARGO_TARGET_DIR", "GITHUB_WORKSPACE", "RUNNER_TEMP", "TMPDIR"):
        if os.environ.get(key):
            paths.add(Path(os.environ[key]))
    needles = set()
    for path in paths:
        for root in (path.absolute(), path.resolve()):
            if root == Path(root.anchor):
                continue
            for prefix in (str(root), root.as_posix()):
                for form in (
                    prefix, html.escape(prefix), quote(prefix), quote(prefix, safe=""),
                    json.dumps(prefix, ensure_ascii=False)[1:-1],
                ):
                    needles.add(form.encode())
    roots = b"|".join(re.escape(needle) for needle in sorted(needles))
    boundary = rb"""(?=$|[/\\\s"'<>`]|%2[Ff]|%5[Cc]|&quot;|&#)"""
    return re.compile(rb"file:///|(?:" + roots + rb")" + boundary)


def package_names(source_root):
    """Select published workspace libraries and the external trait documentation they need."""
    published = {}
    workspace = tomllib.loads((source_root / "Cargo.toml").read_text())["workspace"]
    for member in workspace["members"]:
        manifest = tomllib.loads((source_root / member / "Cargo.toml").read_text())
        if manifest["package"].get("publish") is not False:
            package = manifest["package"]["name"]
            published[package] = manifest.get("lib", {}).get("name", package).replace("-", "_")
    locked = tomllib.loads((source_root / "Cargo.lock").read_text())["package"]
    external = ["futures-core"] if any(p["name"] == "futures-core" for p in locked) else []
    return published, external


def cargo_commands(source_root):
    """Build inherited trait links first, then the public libraries from this checkout."""
    published, external = package_names(source_root)
    base = ["cargo", "doc", "--locked", "--no-deps"]
    commands = [[*base, "-p", name] for name in external]
    if "rca-event-listener" in published:
        commands.append([*base, "--all-features", "-p", "rca-event-listener"])
    commands.append([*base, "--all-features", "--lib", *(arg for name in published for arg in ("-p", name))])
    return commands


def link_tokio_pin_methods(output, source_root):
    """Resolve Tokio links inherited by the facades' Pin aliases under --no-deps."""
    script = output / "type.impl/core/pin/struct.Pin.js"
    if not script.is_file():
        return
    methods = (("AsyncRead", "poll_read", "tymethod"), ("AsyncWrite", "poll_write", "tymethod"),
               ("AsyncWrite", "poll_write_vectored", "method"))
    original = script.read_bytes()
    links = {
        f'<a href=\\"{trait}::{method}\\">'.encode(): f'io/trait.{trait}.html#{kind}.{method}\\">'.encode()
        for trait, method, kind in methods
    }
    if not any(link in original for link in links):
        return  # Another rustdoc version or documentation of the dependency resolved the links.
    # Keep the byte boundaries rustdoc uses to replace individual crates on a later build.
    body, _, footer = original.rpartition(b"\n//")
    try:
        metadata = json.loads(footer)
        offset = metadata["start"]
        if not body[:offset].endswith(b"Object.fromEntries(["):
            raise ValueError("Unrecognised header")
        rewritten = bytearray(body[:offset])
        for i, length in enumerate(metadata["fragment_lengths"]):
            fragment = body[offset:offset + length]
            name, _ = json.loads(fragment.removeprefix(b","))
            offset += length
            if name in {"rca", "rust_composable_architecture"} and any(link in fragment for link in links):
                packages = tomllib.loads((source_root / "Cargo.lock").read_text())["package"]
                versions = {p["version"] for p in packages if p["name"] == "tokio"}
                if len(versions) != 1 or any(fragment.count(link) > 1 for link in links):
                    raise ValueError("Ambiguous inherited Tokio method")
                url = rb'<a href=\"https://docs.rs/tokio/' + quote(versions.pop(), safe="").encode() + b"/tokio/"
                for link, target in links.items():
                    fragment = fragment.replace(link, url + target)
            metadata["fragment_lengths"][i] = len(fragment)
            rewritten.extend(fragment)
        if not body[offset:].startswith(b"]);"):
            raise ValueError("Unrecognised footer")
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError("Unexpected rustdoc Pin implementation index.") from error
    rewritten.extend(body[offset:])
    if rewritten != body:
        script.write_bytes(rewritten + b"\n//" + json.dumps(metadata, separators=(",", ":")).encode())


def prepare(output, source_root=ROOT):
    """Prepare the site without changing rendered Rust source or supported API paths."""
    crate = output / "rca"
    src = output / "src"
    if not (crate / "index.html").is_file() or not src.is_dir():
        raise ValueError("Missing RCA documentation or source pages.")

    # rustdoc retains crate pages from earlier builds in the same target directory.
    published, external = package_names(source_root)
    expected = set(published.values()) | {name.replace("-", "_") for name in external}
    actual = {page.parent.name for page in output.glob("*/index.html")}
    if expected != actual:
        raise ValueError(
            f"Unexpected rustdoc inventory: missing {sorted(expected - actual)}; "
            f"extra {sorted(actual - expected)}. Build in a fresh documentation directory."
        )

    for page in src.rglob("*.rs.html"):
        source = page.relative_to(src).with_suffix("").as_posix()
        public_name = html.escape(source).encode()
        original = page.read_bytes()
        rendered, count = SOURCE_DESCRIPTION.subn(
            lambda match: match[1] + public_name + match[2], original
        )
        if count != 1:
            raise ValueError(f"Unexpected rustdoc source description: {page.relative_to(output)}")
        if original != rendered:
            page.write_bytes(rendered)

    link_tokio_pin_methods(output, source_root)
    for licence in ("LICENSE-APACHE.md", "LICENSE-MIT.md"):
        shutil.copyfile(source_root / licence, crate / licence)
    (output / "index.html").write_text(LANDING, encoding="utf-8")

    forbidden = private_paths(output, source_root)
    for path in output.rglob("*"):
        if path.is_file() and forbidden.search(path.read_bytes()):
            raise ValueError(f"Local build path in published file: {path.relative_to(output)}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="rustdoc output directory")
    parser.add_argument("--source-root", type=Path, default=ROOT, help="checkout used to build the documentation")
    parser.add_argument("--compile-only", action="store_true", help="compile the public libraries inside the isolated build container")
    args = parser.parse_args()
    try:
        if args.compile_only:
            if sys.platform != "linux" or os.environ.get("CARGO_TARGET_DIR") != "/rca-target":
                parser.error("--compile-only runs inside the documentation build container")
            for command in cargo_commands(args.source_root):
                subprocess.run(command, cwd=args.source_root, check=True)
        else:
            prepare(args.output, args.source_root)
    except ValueError as error:
        parser.exit(1, f"{error}\n")
