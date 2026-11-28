"""Prepare rustdoc's publication tree and reject paths from the build machine."""

import argparse
import html
import json
import os
from pathlib import Path
import re
import shutil
import tempfile
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]
SOURCE_DESCRIPTION = re.compile(rb'(<meta name="description" content="Source of the Rust file `)[^"]*(`\.">)')
LANDING = (
    '<!doctype html><html lang="en"><head><meta charset="utf-8">'
    '<meta http-equiv="refresh" content="0; url=rca/index.html"><title>Rust Composable Architecture</title>'
    '</head><body><a href="rca/index.html">Open the API documentation</a></body></html>\n'
)


def private_paths(output):
    """Match literal and escaped build paths in published files, not in build logs."""
    paths = {Path.home(), Path.cwd(), ROOT, output, Path(tempfile.gettempdir())}
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


def prepare(output):
    """Use published source names in descriptions; leave source text and links intact."""
    crate = output / "rca"
    src = output / "src"
    if not (crate / "index.html").is_file() or not src.is_dir():
        raise ValueError("Missing RCA documentation or source pages.")

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

    for licence in ("LICENSE-APACHE.md", "LICENSE-MIT.md"):
        shutil.copyfile(ROOT / licence, crate / licence)
    (output / "index.html").write_text(LANDING, encoding="utf-8")

    forbidden = private_paths(output)
    for path in output.rglob("*"):
        if path.is_file() and forbidden.search(path.read_bytes()):
            raise ValueError(f"Local build path in published file: {path.relative_to(output)}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="rustdoc output directory")
    try:
        prepare(parser.parse_args().output)
    except ValueError as error:
        parser.exit(1, f"{error}\n")
