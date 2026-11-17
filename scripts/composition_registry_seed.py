"""Share locked crates.io outputs between unmeasured, isolated benchmark builds in one capture."""

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import tarfile
import tomllib


REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def _sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _direct(path):
    if not path.is_absolute() or path.resolve() != path or path.is_symlink():
        raise ValueError(f"redirected registry preparation path: {path}")
    return path


def _regular(path):
    mode = _direct(path).stat().st_mode
    if not stat.S_ISREG(mode):
        raise ValueError(f"expected a regular preparation file: {path}")
    return path


def verify_registry_source(directory, archive, checksum):
    """Match the unpacked crate Cargo compiled to every file in its locked archive."""
    directory = _direct(directory)
    if _sha(_regular(archive)) != checksum:
        raise ValueError("registry archive checksum differs from the source lock")
    expected = set()
    with tarfile.open(archive, "r|gz") as stream:
        for member in stream:
            parts = PurePosixPath(member.name).parts
            if (not parts or parts[0] != directory.name or ".." in parts or "\\" in member.name
                    or any(":" in part for part in parts)):
                raise ValueError("registry archive member escaped its package")
            if member.isdir():
                continue
            path = directory.joinpath(*parts[1:])
            if not member.isfile() or path in expected:
                raise ValueError("unexpected registry archive member")
            with stream.extractfile(member) as saved:
                if _sha(_regular(path)) != hashlib.file_digest(saved, "sha256").hexdigest():
                    raise ValueError("unpacked registry source differs from the locked archive")
            expected.add(path)
    actual = {p for p in directory.rglob("*") if not _direct(p).is_dir()}
    if actual - expected - {directory / ".cargo-ok", directory / ".cargo-checksum.json"}:
        raise ValueError("unpacked registry package contains an unpinned file")
    return actual


def _locks(target, digest):
    lock = _regular(target.parent / f"source-{target.name}" / "Cargo.lock")
    if _sha(lock) != digest:
        raise ValueError("benchmark source lock changed")
    return {f"{REGISTRY}#{p['name']}@{p['version']}": p for p in tomllib.loads(lock.read_text(encoding="utf-8"))["package"]
            if p.get("source") == REGISTRY}


def _records(log):
    rows = [json.loads(line) for line in _regular(log).read_text(encoding="utf-8").splitlines() if line.startswith("{")]
    if (not rows or rows[-1].get("reason") != "build-finished" or rows[-1].get("success") is not True
            or sum(r.get("reason") == "build-finished" for r in rows) != 1):
        raise ValueError("missing successful Cargo benchmark build")
    return rows


def _outputs(row):
    if row["reason"] == "build-script-executed":
        return [Path(row["out_dir"])]
    paths = row["filenames"]
    if not isinstance(paths, list) or not paths:
        raise ValueError("missing registry compiler outputs")
    return [Path(path) for path in paths]


def verify_rebuild(log, allowed):
    """Reject a warm owned unit or a unit not copied for this build."""
    hits = 0
    benches = 0
    for row in _records(log):
        if row.get("reason") != "compiler-artifact":
            continue
        fresh = row.get("fresh")
        if type(fresh) is not bool:
            raise ValueError("Cargo did not report whether a benchmark unit was reused")
        if row["target"]["name"] == "composition" and "bench" in row["target"]["kind"]:
            if fresh or not row.get("executable") or not row["package_id"].startswith("path+"):
                raise ValueError("the owned composition benchmark did not compile afresh")
            benches += 1
        if fresh:
            for file in _outputs(row):
                sealed = allowed.get(_direct(file))
                if not sealed or sealed[0] != row["package_id"]:
                    raise ValueError(f"reused a unit outside the copied registry set: {row['package_id']}")
                if _sha(_regular(file)) != sealed[1]:
                    raise ValueError("Cargo reused a file that no longer matches the sealed copy")
            hits += 1
    if benches != 1:
        raise ValueError("missing a single freshly compiled composition benchmark")
    return hits


@dataclass
class _Seed:
    target: Path
    locks: dict
    units: dict


def prepare_seed(donor_target, donor_log, cargo_home, *, lock_sha256):
    """Seal files from the first cold control. An unsupported Cargo layout stays cold."""
    donor = _direct(Path(donor_target))
    if donor.parts[-2:] != ("candidate-reference", "control"):
        raise ValueError("only the first normal control may supply registry units")
    private = _direct(donor.parent.parent)
    if os.name == "posix" and (private.stat().st_uid != os.getuid() or private.stat().st_mode & 0o077):
        raise ValueError("registry reuse requires a private capture directory")
    log = Path(donor_log)
    if log.parts[-3:] != ("candidate-reference", "builds", "control.txt"):
        raise ValueError("unexpected donor benchmark log")
    verify_rebuild(log, {})
    locks = _locks(donor, lock_sha256)
    home = _direct(Path(cargo_home))
    checked, entries = {}, {}
    supported = True
    for row in _records(log):
        if row.get("reason") not in ("compiler-artifact", "build-script-executed"):
            continue
        key = row["package_id"]
        if not key.startswith(REGISTRY + "#"):
            continue
        if key not in locks:
            raise ValueError("donor registry unit is not locked")
        package = locks[key]
        if row["reason"] == "compiler-artifact":
            src = _regular(Path(row["target"]["src_path"]))
            parts = src.relative_to(home / "registry/src").parts
            name = f"{package['name']}-{package['version']}"
            if len(parts) < 3 or parts[1] != name:
                raise ValueError("registry compiler input does not belong to the locked package")
            directory = home / "registry/src" / parts[0] / name
            if key not in checked:
                archive = home / "registry/cache" / parts[0] / (name + ".crate")
                verify_registry_source(directory, archive, package["checksum"])
                checked[key] = directory
            elif checked[key] != directory:
                raise ValueError("one registry package compiled from multiple directories")
        for output in _outputs(row):
            parts = _direct(output).relative_to(donor).parts
            if not output.exists():
                raise ValueError("missing donor registry output")
            if (len(parts) < 5 or parts[:2] != ("release", "build") or parts[2] != package["name"]
                    or not re.fullmatch(r"[0-9a-f]{16}", parts[3]) or parts[4] != "out"):
                supported = False
                continue
            unit = Path(*parts[:4])
            if entries.setdefault(unit, key) != key:
                raise ValueError("registry output directory has conflicting owners")
    if not set(entries.values()) <= checked.keys():
        raise ValueError("registry build script has no checked compiler provenance")
    if not entries or not supported:
        return None
    units = {}
    for unit, key in entries.items():
        directory = _direct(donor / unit)
        files = {}
        for file in directory.rglob("*"):
            if not _direct(file).is_dir():
                files[file.relative_to(directory)] = _sha(_regular(file))
        units[unit] = (key, files)
    return _Seed(donor, locks, units)


def seed_benchmark(seed, target, *, lock_sha256):
    """Copy sealed registry bytes, never links, only to a new normal preparation target."""
    target = _direct(Path(target))
    if (target.name not in ("control", "subject")
            or target.parent.name not in ("candidate-reference", "reference-reference-aa")
            or target.exists()):
        raise ValueError("only a new normal benchmark target may be seeded")
    if seed is None:
        return {}
    if target == seed.target or target.parent.parent != seed.target.parent.parent:
        raise ValueError("registry units must stay in the same capture")
    locks = _locks(target, lock_sha256)
    allowed = {}
    for unit, (key, files) in seed.units.items():
        if key not in locks:
            continue
        if locks[key]["checksum"] != seed.locks[key]["checksum"]:
            raise ValueError("role locks disagree about the same registry package")
        for relative, digest in files.items():
            src = _regular(seed.target / unit / relative)
            dst = _direct(target / unit / relative)
            dst.parent.mkdir(parents=True, exist_ok=True)
            if dst.exists():
                raise ValueError("registry copy would overwrite an existing file")
            shutil.copy2(src, dst)
            if src.samefile(dst) or _sha(_regular(dst)) != digest:
                raise ValueError("registry unit changed since the donor build")
            allowed[dst] = (key, digest)
    return allowed
