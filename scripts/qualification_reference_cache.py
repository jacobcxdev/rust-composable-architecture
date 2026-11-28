"""Check local Windows reference images on the release clock. Never cache candidate work.

The enrolment digest comes from a separate, trusted record made after replaying the
original builds. A digest supplied by the cache itself establishes no provenance.
The caller derives current inputs from the signed sources and installed toolchain.
This reader cannot enrol images, skip a measured cold build or issue a verdict.
"""

import hashlib
import json
import os
from pathlib import Path, PureWindowsPath
import re

from release_baseline import require_baseline
from composition_registry_seed import _regular, _sha


INPUT_HASHES = ("effective_source_sha256", "lock_sha256", "workload_sha256", "toolchain_sha256",
                "configuration_sha256", "platform_sha256")


def _hex(value, length=64):
    return isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{length}}}", value) is not None


def _origin(text):
    path = PureWindowsPath(text)
    if (not path.is_absolute() or not re.fullmatch("[a-zA-Z]:", path.drive)
            or any(p.endswith((".", " ")) or ":" in p or "~" in p for p in path.parts[1:])):
        raise ValueError("invalid reference build origin")
    return path


def _git_root(path):
    marker = path / ".git"
    return (marker.exists() or marker.is_symlink() or path.name.lower() == ".git"
            or ((path / "HEAD").is_file() and (path / "objects").is_dir() and (path / "refs").is_dir()))


def verify_reference_products(cache, *, enrolment_sha256, inputs, purpose, names, roles=("a", "b")):
    """Return checked reference paths for this workload and these inputs, or raise.

    An identical executable can result from two independent builds. Check their
    independently enrolled origins rather than requiring different machine code.
    """
    cache = Path(cache)
    if not cache.is_absolute() or cache.resolve(strict=True) != cache or not cache.is_dir():
        raise ValueError("expected a direct, absolute local reference cache directory")
    if any(_git_root(p) for p in (cache, *cache.parents)):
        raise ValueError("reference build outputs must be outside Git worktrees")
    if os.name == "nt":
        import ctypes
        if (not re.fullmatch("[a-zA-Z]:", cache.drive)
                or ctypes.windll.kernel32.GetDriveTypeW(ctypes.c_wchar_p(cache.anchor)) != 3):
            raise ValueError("reference cache requires a local fixed Windows volume")
    if (not _hex(enrolment_sha256) or set(inputs) != {"tree", *INPUT_HASHES}
            or not _hex(inputs["tree"], 40) or not all(_hex(inputs[k]) for k in INPUT_HASHES)):
        raise ValueError("incomplete current reference inputs or enrolment pin")
    if (not roles or len(roles) != len(set(roles)) or not set(roles) <= {"a", "b"}
            or not names or len(names) != len(set(names))
            or not all(re.fullmatch("[a-z0-9][a-z0-9_-]*", n) for n in (purpose, *names))):
        raise ValueError("expected fixed reference roles and product names")

    encoded = _regular(cache / "enrolment.json").read_bytes()
    if hashlib.sha256(encoded).hexdigest() != enrolment_sha256:
        raise ValueError("enrolment does not match the independent trust pin")
    manifest = json.loads(encoded)
    release = require_baseline()
    if (set(manifest) != {"schema", "revision", "inputs", "purpose", "roles"}
            or type(manifest["schema"]) is not int or manifest["schema"] != 1
            or manifest["revision"] != release.revision or inputs["tree"] != release.tree or manifest["inputs"] != inputs
            or manifest["purpose"] != purpose or set(manifest["roles"]) != set(roles)):
        raise ValueError("wrong reference revision, inputs, purpose or roles")

    results = {}
    sources, targets, proofs = set(), set(), set()
    for role in roles:
        if _git_root(cache / role):
            raise ValueError("reference files cannot live in a nested Git worktree")
        row = manifest["roles"][role]
        if (set(row) != {"source_root", "target_root", "provenance_sha256", "products"}
                or set(row["products"]) != set(names)):
            raise ValueError("incomplete reference build or product list")
        source, target = map(_origin, (row["source_root"], row["target_root"]))
        proof = row["provenance_sha256"]
        if source in sources or target in targets or proof in proofs or source == target:
            raise ValueError("reference controls require independently verified builds")
        if not _hex(proof) or _sha(_regular(cache / role / "provenance.json")) != proof:
            raise ValueError("reference build evidence changed")
        sources.add(source)
        targets.add(target)
        proofs.add(proof)
        products = {}
        for name in names:
            path = _regular(cache / role / (name + ".exe"))
            if _sha(path) != row["products"][name]:
                raise ValueError(f"reference executable changed: {role}/{name}")
            products[name] = path
        results[role] = products
    return results
