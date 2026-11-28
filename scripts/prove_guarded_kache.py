"""Exercise RCA's real Darwin watchdog without changing production build settings."""

import argparse
import ctypes
import sys
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

import guarded_kache as cache
import guard_cargo_memory as guard


MIB = 1024**2
NAME = "rca_guard_kache_probe"
SOURCE = """//! Gives the memory sampler time to observe a real compiler with constant storage.
#![allow(long_running_const_eval, reason = "bounded sampling window")]

/// Performs bounded CPU work at compile time without a heap or recursion.
pub const PROBE_CHECKSUM: u32 = {
    let mut index = 0_u32;
    let mut accumulator = 0_u32;
    while index < 1_000_000 {
        accumulator ^= index;
        index += 1;
    }
    accumulator
};
"""


def require(check, message):
    if not check:
        raise RuntimeError(message)


def proof(args, root, record):
    require(args.command in ("build", "check"), "unsupported Cargo command")
    record["cargo_command"] = args.command
    real = guard.MacProbe()
    headroom = real.host()
    record.update(preflight=headroom, guard_sha256=guard.SOURCE_SHA256, policy_sha256=cache.POLICY_SHA256,
                  host=cache.host_identity())
    safe_cap = 512 * MIB
    admission = guard.host_reason(headroom, safe_cap + guard.HOST_RESERVE)
    require(not admission, f"admission: {admission}")
    require(shutil.disk_usage(root).free > 4 * 1024**3, "disk reserve")
    project = root / "project"
    (project / "src").mkdir(parents=True)
    (project / "Cargo.toml").write_text(
        f'[package]\nname="{NAME}"\nversion="0.1.0"\nedition="2024"\n'
        '[profile.dev]\ndebug=0\nincremental=false\n'
    )
    (project / "src/lib.rs").write_text(SOURCE)
    env = {k: os.environ[k] for k in
           ("PATH", "HOME", "USER", "LOGNAME", "RUSTUP_HOME", "DEVELOPER_DIR", "SDKROOT") if k in os.environ}
    env.update(LANG="C", CARGO_INCREMENTAL="0", CARGO_NET_OFFLINE="true",
               CARGO_HOME=str(root / "cargo"), KACHE_HOST_CONFIG="", KACHE_LOG_FILE="kache=debug", KACHE_BUILD_SCRIPT_CACHE="0",
               XDG_DATA_HOME=str(root / "data"), XDG_CONFIG_HOME=str(root / "config"),
               XDG_CACHE_HOME=str(root / "cache"), TMPDIR=str(root / "scratch"))
    (root / "scratch").mkdir()
    command, env = guard.cargo_command([args.command, "--offline", "--lib", "--message-format=json"], env)
    compiler = Path(env["RUSTC"]).resolve()
    kache = args.kache.resolve(strict=True)
    version = subprocess.check_output([str(kache), "--version"], env=env, text=True, timeout=5).strip()
    require(version == cache.KACHE_VERSION, "unexpected kache version")
    record["version"] = version
    base = env.copy()
    record.update(rustc=str(compiler), rustc_sha256=cache.digest(compiler),
                  kache=str(kache), kache_sha256=cache.digest(kache))

    def daemon_snapshot():
        """Observe existing daemon identities without contacting or starting a service."""
        result = subprocess.run(
            ["/usr/bin/pgrep", "-u", str(os.getuid()), "-f",
             "^(" + re.escape(str(kache)) + "|" + re.escape(str(root)) + "/[^ ]+/kache) daemon run$"],
            text=True, capture_output=True, timeout=2,
        )
        require(result.returncode in (0, 1), "could not identify existing daemons")
        snapshot = {}
        for pid in map(int, result.stdout.split()):
            info = real._info(pid)
            usage = guard.RusageInfo()
            require(info is not None and info.status != 5 and info.uid == os.getuid() and
                    not real.lib.proc_pid_rusage(pid, 0, ctypes.byref(usage)), "daemon identity changed")
            snapshot[pid] = (info.pgid, usage.start_abstime)
        return snapshot

    daemons_before = daemon_snapshot()

    def isolate(name):
        instance = root / name
        instance.mkdir()
        env.clear()
        env.update(cache.kache_environment(base, kache, instance, instance / "store", record["kache_sha256"]))
        env["KACHE_LOG_FILE"] = "kache=debug"
        return instance / "run/events.jsonl"

    class Witness(guard.MacProbe):
        """Record the precise Darwin readings used by the guard; never alter them."""

        def __init__(self):
            super().__init__()
            self.samples = []
            self.lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
            self.lib.proc_pidpath.restype = ctypes.c_int
            original = self.lib.proc_pid_rusage
            self.byte_counts = {}

            def rusage(pid, flavor, buffer):
                result = original(pid, flavor, buffer)
                if not result:
                    self.byte_counts[pid] = ctypes.cast(buffer, ctypes.POINTER(guard.RusageInfo)).contents.phys_footprint
                return result

            self.lib.proc_pid_rusage = rusage

        def group(self, pgid):
            self.byte_counts.clear()
            counters = super().group(pgid)
            total = counters["footprint_bytes"]
            require(sum(self.byte_counts.values()) == total, "per-PID readings do not reconcile")
            sample = {"pgid": pgid, "total": total, "compiler_bytes": 0}
            for pid, footprint in self.byte_counts.items():
                path = ctypes.create_string_buffer(4096)
                if self.lib.proc_pidpath(pid, path, len(path)) <= 0 or os.fsdecode(path.value) != str(compiler):
                    continue
                cmd = subprocess.run(["/bin/ps", "-ww", "-p", str(pid), "-o", "command="],
                                     text=True, capture_output=True, timeout=1).stdout
                if not re.search(rf"(?:^| )--crate-name {NAME}(?: |$)", cmd):
                    continue
                emits = {kind for val in re.findall(r"--emit[= ](\S+)", cmd) for kind in val.split(",")}
                if args.command == "build" and "link" not in emits:
                    continue
                if args.command == "check" and ("metadata" not in emits or "link" in emits):
                    continue  # Neither command accepts a preliminary dep-info-only rustc.
                info = self._info(pid)
                if info is None or info.status == 5:
                    continue
                require(info.pgid == pgid and info.uid == os.getuid(), "compiler ownership changed")
                parent_path = ctypes.create_string_buffer(4096)
                require(self.lib.proc_pidpath(info.ppid, parent_path, len(parent_path)) > 0 and
                        os.fsdecode(parent_path.value) == env["RUSTC_WRAPPER"], "real compiler did not belong to kache")
                sample.update(rustc_pid=pid, kache_pid=info.ppid, compiler_bytes=footprint)
            self.samples.append(sample)
            return counters

    def run(stage, cap, logpath):
        log = root / (stage + ".log")
        klog = root / (stage + ".kache.log")
        env.update(CARGO_TARGET_DIR=str(project / "target"), KACHE_LOG_FILE_PATH=str(klog))
        before = logpath.stat().st_size if logpath.exists() else 0
        probe = Witness()
        with log.open("wb") as stream:
            receipt = guard.run_guarded(command, cwd=project, env=env, probe=probe, timeout=45,
                                        memory_limit=cap, stdout=stream, stderr=stream)
        slim = {k: receipt.get(k) for k in ("reason","started","returncode","cleanup_complete","forced_cleanup","pid",
                                             "peak_footprint_bytes","memory_limit_bytes","host_reserve_bytes","error")}
        record[stage] = slim
        require(slim["cleanup_complete"] and slim["started"], f"{stage}: {slim}")
        events = []
        if logpath.exists():
            with logpath.open("rb") as stream:
                stream.seek(before)
                for line in stream:
                    event = json.loads(line)
                    if event.get("crate_name") == NAME and "result" in event:
                        events.append({k:event.get(k) for k in ("result","compiler_runs","cache_key")})
        slim.update(events=events)
        if receipt["reason"] not in ("command-exited","memory-limit"):
            raise RuntimeError(f"{stage}: {slim}, {log.read_text()[-700:]}")
        if stage != "abort":
            require(receipt["returncode"] == 0 and not receipt["forced_cleanup"], f"{stage}: {slim} {log.read_text()[-700:]}")
        else:
            require(klog.exists() and f"cache miss for {NAME}, compiling" in klog.read_text(),
                    "did not see kache's real cache-miss decision")
        return probe, slim

    # One bounded calibration on a different store chooses a low realistic ceiling.
    logpath = isolate("calibration")
    probe, cold = run("calibrate", safe_cap, logpath)
    require(len(cold["events"]) == 1 and cold["events"][0]["result"] == "miss", "calibration not a real miss")
    rows = probe.samples
    require(any(s["compiler_bytes"] for s in rows), "real compile too short to sample")
    first = next(i for i,s in enumerate(rows) if s["compiler_bytes"])
    best = max((s for s in rows if s["compiler_bytes"]), key=lambda s:s["total"])
    low = max([s["total"] for s in rows[:first]] + [best["total"]-best["compiler_bytes"]])
    require(best["total"]-low >= 4*MIB, "cannot distinguish compiler from other process memory")
    limit = (best["total"] + low) // 2
    require(0 < limit < safe_cap, "invalid test limit")
    (project/"target").rename(project/"finished-calibration")
    logpath = isolate("measured")
    probe, killed = run("abort", limit, logpath)
    require(killed["reason"] == "memory-limit" and killed["forced_cleanup"], "guard did not stop group for memory")
    sample = probe.samples[-1]
    require(sample["total"] >= limit > sample["total"]-sample["compiler_bytes"], "stop not attributable to rustc")
    require(guard.MacProbe().group(sample["pgid"])["process_count"] == 0, "owned group survived cleanup")
    for pid in (sample["rustc_pid"], sample["kache_pid"]):
        info = real._info(pid)
        require(info is None or info.status == 5, "recorded compiler or wrapper survived")
    killed.update(threshold_sample=sample, live_after=0)
    (project/"target").rename(project/"aborted-target")
    _, retry = run("retry", safe_cap, logpath)
    require(len(retry["events"]) == 1 and retry["events"][0]["result"] == "miss"
            and retry["events"][0]["compiler_runs"] == 1, "retry did not compile a real miss")
    def artifact(stage):
        suffix = ".rlib" if args.command == "build" else ".rmeta"
        matches = []
        for line in (root / (stage + ".log")).read_text().splitlines():
            if not line.startswith("{"):
                continue
            row = json.loads(line)
            if row.get("reason") == "compiler-artifact" and row["target"]["name"] == NAME:
                require(row["fresh"] is False, "Cargo reused existing target output")
                matches.extend(Path(f) for f in row["filenames"] if f.endswith(suffix))
        require(len(matches) == 1 and matches[0].is_relative_to(project / "target"), f"unexpected Cargo artifacts: {matches}")
        return matches[0]

    digest = cache.digest(artifact("retry"))
    (project/"target").rename(project/"finished-cold")
    _, warm = run("warm", safe_cap, logpath)
    require(len(warm["events"]) == 1 and warm["events"][0]["result"] == "local_hit"
            and warm["events"][0]["compiler_runs"] == 0, "fresh target did not restore a local hit")
    key = retry["events"][0]["cache_key"]
    require(isinstance(key, str) and bool(key) and warm["events"][0]["cache_key"] == key, "key mismatch")
    require(cache.digest(artifact("warm")) == digest, "restored artifact changed")
    require(daemon_snapshot() == daemons_before, "daemon set or identities changed during proof")
    record.update(outcome="pass", artifact_sha256=digest, existing_daemons_unchanged=len(daemons_before))


if __name__ == "__main__":
    cli = argparse.ArgumentParser()
    cli.add_argument("--kache", type=Path, default=shutil.which("kache"), help="defaults to kache on PATH")
    cli.add_argument("--command", choices=("build", "check"), default="build")
    args = cli.parse_args()
    if sys.version_info < (3, 11):
        cli.error("kache proof requires Python 3.11 or later")
    if args.kache is None:
        cli.error("kache is not on PATH")
    output = cache.proof_path(args.command)
    record = {"outcome": "incomplete"}
    try:
        with tempfile.TemporaryDirectory(prefix="rca-kp-", dir="/private/tmp") as directory:
            proof(args, Path(directory), record)
        output.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        with tempfile.NamedTemporaryFile(dir=output.parent, mode="w", encoding="utf-8",
                                         delete=False) as out:
            staged = Path(out.name)
            try:
                print(json.dumps(record, indent=2), file=out)
                out.flush()
                staged.replace(output)
            finally:
                staged.unlink(missing_ok=True)
    except Exception as error:
        record["outcome"] = "incomplete"
        record["error"] = str(error)
    print(json.dumps(record, sort_keys=True), file=sys.stdout)
    if record["outcome"] == "pass":
        print(f"Installed {args.command} proof: {output}", file=sys.stderr)
    raise SystemExit(0 if record["outcome"] == "pass" else 1)
