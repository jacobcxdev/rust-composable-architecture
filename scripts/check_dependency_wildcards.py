#!/usr/bin/env python3
"""Narrow cargo-deny's allow-wildcard-paths setting to two unpublished test edges."""

import json
from pathlib import Path
import sys
import tomllib


def check(metadata):
    crates = Path(metadata["workspace_root"]) / "crates"
    macros = crates / "rca-macros" / "Cargo.toml"
    packages = {package["id"]: package for package in metadata["packages"]}
    allowed = {"rca": "rca-core", "rca-test": "rca-test"}
    for node in metadata["resolve"]["nodes"]:
        owner = packages[node["id"]]
        if (owner["source"] or "").startswith("git+"):
            continue  # cargo-deny skips manifests from Git regardless of the setting.
        for dep in owner["dependencies"]:
            if dep["req"] != "*" or (owner["publish"] != [] and dep["kind"] != "dev"):
                continue
            alias = dep["rename"] or dep["name"]
            for edge in node["deps"]:
                target = packages[edge["pkg"]]
                if (target is owner or target["name"] != dep["name"]
                        or (target["source"] and not target["source"].startswith("git+"))
                        or (dep["rename"] and edge["name"] != alias.replace("-", "_"))
                        or {"kind": dep["kind"], "target": dep["target"]} not in edge["dep_kinds"]):
                    continue
                if (owner["source"] is None and Path(owner["manifest_path"]) == macros
                        and owner["name"] == "rca-macros" and allowed.get(alias) == dep["name"]
                        and dep["kind"] == "dev" and dep["target"] is None and dep["source"] is None
                        and dep.get("path") == str(crates / dep["name"])
                        and Path(target["manifest_path"]) == crates / dep["name"] / "Cargo.toml"):
                    # Metadata reports both an omitted version and an explicit `*` as `*`.
                    with macros.open("rb") as source:
                        declared = tomllib.load(source)["dev-dependencies"][alias]
                    if isinstance(declared, dict) and "path" in declared and "version" not in declared:
                        continue
                raise ValueError(f"{owner['name']}: wildcard {dep['kind'] or 'normal'} dependency {alias}")


if __name__ == "__main__":
    try:
        check(json.load(sys.stdin))
    except ValueError as error:
        sys.exit(str(error))
