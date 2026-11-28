#!/usr/bin/env python3
"""Read the approved private profile for a release runner."""

import argparse
import hashlib
import json
import os
from pathlib import Path


# These digests bind qualification to the reviewed host and limits, not to a path.
PROFILE_SHA256 = {
    "macos": "341f29dd465b15cf9600632d0902eb5103e7445c56f8f9fe65f5919ebbcc5c0a",
    "windows": "579d23e7cbf3163c18430ab8b17b7465a9c3791f937fb19f3582af3466f04df8",
}


def load(platform: str) -> dict:
    """Fail before qualification if the private file is missing or has changed."""
    if platform not in PROFILE_SHA256:
        raise ValueError("unknown release runner platform")
    directory = Path(os.environ.get("RCA_RUNNER_PROFILE_DIR", ""))
    if not directory.is_absolute():
        raise ValueError("set RCA_RUNNER_PROFILE_DIR to an absolute directory outside the checkout")
    path = (directory / f"{platform}.json").resolve()
    if path.is_relative_to(Path(__file__).resolve().parents[1]):
        raise ValueError("keep release runner profiles outside the checkout")
    try:
        contents = path.read_bytes()
    except OSError as error:
        raise ValueError(f"cannot read the private {platform} runner profile") from error
    if hashlib.sha256(contents).hexdigest() != PROFILE_SHA256[platform]:
        raise ValueError(f"private {platform} runner profile does not match the approved digest")
    return json.loads(contents)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("platform", choices=PROFILE_SHA256)
    try:
        print(json.dumps(load(parser.parse_args().platform)))
    except ValueError as error:
        parser.exit(1, f"{error}\n")
