#!/usr/bin/env python3
"""Give Cargo a publish token only for archive bytes approved for this release."""

import hashlib
import json
import os
from pathlib import Path
import sys


def credential(request):
    """Check Cargo's actual upload checksum, not an earlier packaging attempt."""
    registry = request["registry"]
    if (type(request["v"]) is not int or request["v"] != 1 or request["kind"] != "get"
            or registry.get("name") not in (None, "crates-io")
            or registry["index-url"] not in ("https://github.com/rust-lang/crates.io-index",
                                             "sparse+https://index.crates.io/")):
        raise ValueError
    if request["operation"] == "read":
        # Cargo checks that it can get a token before packaging. The public index
        # needs no credential. Cargo replaces this value before each real upload.
        token = "rca-public-index"
    elif request["operation"] == "publish":
        name, version = request["name"], request["vers"]
        missing = Path(os.environ["RCA_RELEASE_MISSING"]).read_text().splitlines()
        if name not in missing or f"v{version}" != os.environ["GITHUB_REF_NAME"]:
            raise ValueError
        archives = Path(__file__).resolve().parents[1] / "target" / "package"
        with (archives / f"{name}-{version}.crate").open("rb") as archive:
            expected = hashlib.file_digest(archive, "sha256").hexdigest()
        if request["cksum"] != expected:
            raise ValueError
        token = os.environ["RCA_RELEASE_TOKEN"]
        if not token:
            raise ValueError
    else:
        raise ValueError
    return {"Ok": {"kind": "get", "token": token, "cache": "never", "operation_independent": False}}


if __name__ == "__main__":
    if sys.argv[1:] != ["--cargo-plugin"] or sys.stdout.isatty():
        sys.exit("Only Cargo can request a release credential through its protocol pipe.")
    print('{"v":[1]}', flush=True)
    try:
        response = credential(json.loads(sys.stdin.readline()))
    except (AttributeError, KeyError, OSError, TypeError, ValueError):
        # Cargo shows this message to the user. Never echo the request or token.
        response = {"Err": {"kind": "other", "message": "Credential refused: request did not match the approved release."}}
    print(json.dumps(response))
