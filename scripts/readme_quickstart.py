#!/usr/bin/env python3
"""Copy the literal README quickstart into a standalone Cargo consumer."""

import argparse
import hashlib
import pathlib
import re


def extract(readme: str, keyed: bool = False) -> tuple[str, str]:
    heading = "### Keyed testing\n" if keyed else "## Quick start\n"
    if heading not in readme:
        raise ValueError(f"README section missing: {heading.strip()}")
    quickstart = re.split(r"^#{2,3} ", readme.split(heading, 1)[1], maxsplit=1, flags=re.MULTILINE)[0]
    manifest = re.search(r"```toml\n(.*?)\n```", quickstart, re.DOTALL)
    source = re.search(r"```rust(?:,no_run)?\n(.*?)\n```", quickstart, re.DOTALL)
    if manifest is None or source is None:
        raise ValueError("README quickstart requires a manifest and Rust source")
    return manifest[1] + "\n", source[1] + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("readme", type=pathlib.Path)
    parser.add_argument("destination", type=pathlib.Path)
    parser.add_argument("--keyed", action="store_true")
    args = parser.parse_args()
    manifest, source = extract(args.readme.read_text(encoding="utf-8"), args.keyed)
    args.destination.mkdir(parents=True, exist_ok=False)
    (args.destination / "src").mkdir()
    (args.destination / "Cargo.toml").write_text(manifest, encoding="utf-8")
    (args.destination / "src/main.rs").write_text(source, encoding="utf-8")
    for relative in ("Cargo.toml", "src/main.rs"):
        path = args.destination / relative
        print(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.resolve()}")


if __name__ == "__main__":
    main()
