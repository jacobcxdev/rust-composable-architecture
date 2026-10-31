#!/usr/bin/env python3
"""Run Cargo or canonical Mac qualification with a polling memory watchdog.

The watchdog targets a 250 ms interval between samples. Physical footprint
includes compressed memory. It stops the group at eight GiB, on host memory
pressure, or when estimated reclaimable host memory falls below four GiB.
The estimate counts free and file-backed pages, without counting speculative
pages twice. It is not a promise that all file-backed pages are immediately free.

Only one watchdog can run for each user. Cargo uses one build job and the pinned
toolchain. Compiler wrappers are disabled because an existing cache daemon can
start compilers outside the owned group.
Scheduling delays can let memory exceed the threshold before the next sample.
This is not a sandbox: deliberate daemonisation, SIGKILL and SIGSTOP of the
watchdog are not contained. Use an OS-limited remote job for untrusted or very
large builds.
No memory limit is silently increased and no failed command is retried.

Apple counter definitions:
https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/resource.h
https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/task.c
https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_memorystatus_notify.c
"""

import argparse
import ctypes
from datetime import datetime, timezone
import errno
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import threading
import time


GIB = 1024**3
MEMORY_LIMIT = 8 * GIB
HOST_RESERVE = 4 * GIB
INTERVAL = 0.25
TOOLCHAIN = "1.99.0"
LOCK_PATH = Path(f"/tmp/rca-cargo-memory-{os.getuid()}.lock")
STOP_SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT, signal.SIGTSTP)
BUILD_COMMANDS = {"build", "check", "test", "bench", "doc", "rustc", "rustdoc", "run", "clippy"}
SOURCE_SHA256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def acquire_lock():
    """Hold a per-user kernel lock without following links or replacing a file."""
    descriptor = os.open(LOCK_PATH, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        metadata = os.fstat(descriptor)
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or stat.S_IMODE(metadata.st_mode) != 0o600 or metadata.st_nlink != 1):
            raise ValueError("the watchdog lock must be a regular, owner-only file")
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


class RusageInfo(ctypes.Structure):
    """Match Apple's public rusage_info_v0, including compressed footprint."""

    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64) for name in (
            "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins",
            "wired_size", "resident_size", "phys_footprint", "start_abstime", "exit_abstime",
        )
    ]


class BsdInfo(ctypes.Structure):
    """Match proc_bsdshortinfo to check group membership, owner and exit state."""

    _fields_ = [(name, ctypes.c_uint32) for name in ("pid", "ppid", "pgid", "status")] + [
        ("comm", ctypes.c_char * 16)
    ] + [(name, ctypes.c_uint32) for name in (
        "flags", "uid", "gid", "ruid", "rgid", "svuid", "svgid", "reserved",
    )]


class VmInfo(ctypes.Structure):
    """Read the stable revision-one prefix of vm_statistics64."""

    _fields_ = [(name, ctypes.c_uint32) for name in (
        "free", "active", "inactive", "wired",
    )] + [(name, ctypes.c_uint64) for name in (
        "zero_fill", "reactivations", "pageins", "pageouts", "faults", "cow_faults",
        "lookups", "hits", "purges",
    )] + [("purgeable", ctypes.c_uint32), ("speculative", ctypes.c_uint32)] + [
        (name, ctypes.c_uint64) for name in ("decompressions", "compressions", "swapins", "swapouts")
    ] + [(name, ctypes.c_uint32) for name in (
        "compressor", "throttled", "external", "internal",
    )] + [("uncompressed_in_compressor", ctypes.c_uint64)]


class MacProbe:
    """Read counters directly, without starting a subprocess for each sample."""

    def __init__(self):
        if sys.platform != "darwin":
            raise OSError("the Cargo memory watchdog requires macOS")
        self.lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
        signatures = {
            "sysctlbyname": ([ctypes.c_char_p, ctypes.c_void_p, ctypes.POINTER(ctypes.c_size_t),
                              ctypes.c_void_p, ctypes.c_size_t], ctypes.c_int),
            "proc_listpids": ([ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
            "proc_pidinfo": ([ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                              ctypes.c_void_p, ctypes.c_int], ctypes.c_int),
            "proc_pid_rusage": ([ctypes.c_int, ctypes.c_int, ctypes.c_void_p], ctypes.c_int),
            "mach_host_self": ([], ctypes.c_uint32),
            "mach_port_deallocate": ([ctypes.c_uint32, ctypes.c_uint32], ctypes.c_int),
            "host_statistics64": ([ctypes.c_uint32, ctypes.c_int, ctypes.c_void_p,
                                   ctypes.POINTER(ctypes.c_uint32)], ctypes.c_int),
        }
        for name, (arguments, result) in signatures.items():
            function = getattr(self.lib, name)
            function.argtypes, function.restype = arguments, result
        self.page_size = os.sysconf("SC_PAGE_SIZE")
        self.memory_size = self._sysctl("hw.memsize", ctypes.c_uint64)
        self.task_port = ctypes.c_uint32.in_dll(self.lib, "mach_task_self_").value

    def _sysctl(self, name, kind):
        value = kind()
        size = ctypes.c_size_t(ctypes.sizeof(value))
        if self.lib.sysctlbyname(name.encode(), ctypes.byref(value), ctypes.byref(size), None, 0):
            raise OSError(ctypes.get_errno(), f"could not read {name}")
        if size.value != ctypes.sizeof(value):
            raise ValueError(f"unexpected counter size for {name}")
        return value.value

    def host(self):
        """Return the pressure level and a conservative reclaimable-memory estimate."""
        pressure = self._sysctl("kern.memorystatus_vm_pressure_level", ctypes.c_uint32)
        stats = VmInfo()
        count = ctypes.c_uint32(ctypes.sizeof(stats) // 4)
        host = self.lib.mach_host_self()
        try:
            result = self.lib.host_statistics64(host, 4, ctypes.byref(stats), ctypes.byref(count))
        finally:
            self.lib.mach_port_deallocate(self.task_port, host)
        if result or count.value != ctypes.sizeof(stats) // 4:
            raise OSError("could not read the complete host memory counters")
        if stats.speculative > stats.free or stats.speculative > stats.external:
            raise ValueError("host memory counters are inconsistent")
        headroom = (stats.free + stats.external - stats.speculative) * self.page_size
        if not 0 <= headroom <= self.memory_size:
            raise ValueError("host memory estimate is outside physical memory")
        return {"pressure": pressure, "headroom_bytes": headroom}

    def _info(self, pid):
        info = BsdInfo()
        ctypes.set_errno(0)
        size = self.lib.proc_pidinfo(pid, 13, 0, ctypes.byref(info), ctypes.sizeof(info))
        if size == 0 and ctypes.get_errno() == errno.ESRCH:
            return None
        if size != ctypes.sizeof(info):
            raise OSError(ctypes.get_errno(), f"could not inspect process {pid}")
        return info

    def group(self, pgid):
        """Sum physical footprint for live members of the fresh, owned group."""
        pids = (ctypes.c_int * 4096)()
        ctypes.set_errno(0)
        size = self.lib.proc_listpids(2, pgid, pids, ctypes.sizeof(pids))
        if size < 0 or (size == 0 and ctypes.get_errno()) or size >= ctypes.sizeof(pids) or size % 4:
            raise OSError("could not enumerate the complete owned process group")
        footprint = count = 0
        for pid in pids[:size // 4]:
            if pid <= 0:
                continue
            info = self._info(pid)
            if info is None:
                continue
            if info.pid != pid or info.pgid != pgid or info.uid != os.getuid():
                raise ValueError(f"could not establish ownership of process {pid}")
            if info.status == 5:  # SZOMB: exited but not yet reaped.
                continue
            usage = RusageInfo()
            if self.lib.proc_pid_rusage(pid, 0, ctypes.byref(usage)):
                current = self._info(pid)
                if current is None or current.status == 5:
                    continue
                raise OSError(ctypes.get_errno(), f"could not measure process {pid}")
            footprint += usage.phys_footprint
            count += 1
        return {"footprint_bytes": footprint, "process_count": count}


def stop_group(process):
    """Kill the owned group immediately, then prove that no live member remains."""
    # The leader is not reaped until after killpg. Its PID cannot be reused here.
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except PermissionError:
        # Darwin can report EPERM when the group contains only zombies.
        # The process table below must prove that there are no live members.
        pass
    deadline = time.monotonic() + 5
    while True:
        output = subprocess.check_output(
            ["/bin/ps", "-axo", "pgid=,uid=,stat="], text=True, timeout=2,
        )
        live = False
        for line in output.splitlines():
            group, owner, state = line.split()
            if int(group) == process.pid:
                if int(owner) != os.getuid():
                    raise ValueError("the owned process group changed its owner")
                live |= not state.startswith("Z")
        if not live:
            process.wait(timeout=5)
            return
        if time.monotonic() >= deadline:
            raise TimeoutError("the owned process group did not stop")
        # Keep the leader unreaped until all group members stop. Its reserved PID
        # prevents a later signal from reaching a new, unrelated process group.
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        time.sleep(0.05)


def host_reason(sample, minimum):
    """Reject unknown pressure, elevated pressure and insufficient host headroom."""
    if sample["pressure"] not in (1, 2, 4):
        raise ValueError("the host memory pressure level is unknown")
    if sample["pressure"] != 1:
        return "memory-pressure"
    if not isinstance(sample["headroom_bytes"], int) or sample["headroom_bytes"] < 0:
        raise ValueError("the host memory estimate is invalid")
    if sample["headroom_bytes"] < minimum:
        return "host-headroom"
    return None


def require_guarded_group():
    """Reject accidental shell entry outside the live watchdog's owned group.

    This is an entry-point check, not a security boundary against forged process
    state. The owner marker alone is insufficient: the current group leader
    must still be a live child of that owner, with the same user identity.
    """
    message = ("Run this script through RCA's memory-guarded controller; "
               "direct invocation is disabled.")
    try:
        owner = int(os.environ.get("RCA_MAC_MEMORY_GUARD_PID", "0"))
        group = os.getpgrp()
        if (owner <= 0 or owner == group
                or os.environ.get("RCA_MAC_MEMORY_GUARD_SHA256") != SOURCE_SHA256):
            raise ValueError(message)
        snapshot = subprocess.check_output(
            ["/bin/ps", "-p", f"{owner},{group}", "-o", "pid=,ppid=,pgid=,uid=,stat="],
            text=True, stderr=subprocess.PIPE, timeout=2,
        )
        rows = {}
        for line in snapshot.splitlines():
            pid, parent, pgid, uid, state = line.split()
            rows[int(pid)] = (int(parent), int(pgid), int(uid), state)
        if (set(rows) != {owner, group} or rows[group][:3] != (owner, group, os.getuid())
                or rows[owner][2] != os.getuid() or rows[owner][1] == group
                or any(row[3][0] in "ZTX" for row in rows.values())):
            raise ValueError(message)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise ValueError(message) from error


def run_guarded(command, *, cwd, env, probe, timeout=3600, memory_limit=MEMORY_LIMIT,
                reserve=HOST_RESERVE, stdout=None, stderr=None, observe=None, observe_interval=1):
    """Own one process group while the main thread watches its memory.

    A controller may supply a bounded telemetry callback. It runs on one worker,
    never on the memory-sampling thread. Cleanup kills the process group before
    waiting for an active sample, whose failure still invalidates the result.
    The controller must bound every callback; HostMonitor uses 30 seconds.
    ``timeout=None`` preserves an existing controller's unlimited command time.
    """
    if type(memory_limit) is not int or not 0 < memory_limit <= MEMORY_LIMIT:
        raise ValueError("the memory limit must be positive and at most eight GiB")
    if type(reserve) is not int or reserve <= 0:
        raise ValueError("the host memory reserve must be positive")
    if timeout is not None and (not math.isfinite(timeout) or timeout <= 0):
        raise ValueError("the timeout must be finite and positive")
    if not math.isfinite(observe_interval) or observe_interval <= 0:
        raise ValueError("the observation interval must be finite and positive")
    started = time.monotonic()
    receipt = {
        "protection": "polling-watchdog-not-kernel-memory-cap", "started": False,
        "recorded_at": datetime.now(timezone.utc).isoformat(), "returncode": 125,
        "reason": "measurement-error", "peak_footprint_bytes": 0, "peak_process_count": 0,
        "memory_limit_bytes": memory_limit, "host_reserve_bytes": reserve,
        "sample_interval_seconds": INTERVAL, "cleanup_complete": True,
        "memory_watchdog_sha256": SOURCE_SHA256,
        "descendants_complete": False,
        "forced_cleanup": False,
    }
    process = None
    lock = None
    handlers = {}
    received_signal = None
    observer = None
    observer_stop = threading.Event()
    observer_errors = []

    def observe_active():
        try:
            while True:
                observe()
                if observer_stop.wait(observe_interval):
                    return
        except BaseException as error:
            observer_errors.append(f"{type(error).__name__}: {error}")

    def request_stop(number, frame):
        nonlocal received_signal
        # Do not raise between Popen creating the child and assigning its handle.
        received_signal = number

    try:
        for number in STOP_SIGNALS:
            handlers[number] = signal.getsignal(number)
            signal.signal(number, request_stop)
        lock = acquire_lock()
        receipt["initial_host"] = probe.host()
        reason = host_reason(receipt["initial_host"], memory_limit + reserve)
        if reason:
            receipt["reason"] = reason
            return receipt
        if received_signal is not None:
            receipt.update(reason="signal", signal=received_signal, returncode=128 + received_signal)
            return receipt
        child_env = dict(env, RCA_MAC_MEMORY_GUARD_PID=str(os.getpid()),
                         RCA_MAC_MEMORY_GUARD_SHA256=SOURCE_SHA256)
        process = subprocess.Popen(command, cwd=cwd, env=child_env, stdout=stdout, stderr=stderr,
                                   start_new_session=True)
        receipt.update(started=True, pid=process.pid, cleanup_complete=False)
        if observe is not None:
            worker = threading.Thread(target=observe_active, name="rca-host-telemetry")
            worker.start()
            observer = worker
        while True:
            if received_signal is not None:
                receipt.update(reason="signal", signal=received_signal, returncode=128 + received_signal)
                break
            receipt["last_host"] = probe.host()
            reason = host_reason(receipt["last_host"], reserve)
            if reason:
                receipt["reason"] = reason
                break
            group = probe.group(process.pid)
            if (type(group["footprint_bytes"]) is not int or group["footprint_bytes"] < 0
                    or type(group["process_count"]) is not int or group["process_count"] < 0):
                raise ValueError("the owned process counters are invalid")
            receipt["peak_footprint_bytes"] = max(receipt["peak_footprint_bytes"], group["footprint_bytes"])
            receipt["peak_process_count"] = max(receipt["peak_process_count"], group["process_count"])
            if group["footprint_bytes"] >= memory_limit:
                receipt["reason"] = "memory-limit"
                break
            if observer_errors:
                receipt.update(reason="observation-error", error=observer_errors[0])
                if timeout is not None and time.monotonic() - started >= timeout:
                    receipt["reason"] = "timeout"
                break
            status = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
            if group["process_count"] == 0 and status is None:
                raise ValueError("a live command has no measured process group")
            if status is not None:
                receipt["reason"] = "command-exited"
                receipt["returncode"] = status.si_status if status.si_code == os.CLD_EXITED else -status.si_status
                # The first sample may precede the leader's exit. Check again
                # without reaping it: cleanup must not turn leftover work into
                # a successful measurement merely by killing that work.
                descendants = probe.group(process.pid)
                if type(descendants["process_count"]) is not int or descendants["process_count"] < 0:
                    raise ValueError("the final owned process count is invalid")
                receipt["descendants_complete"] = descendants["process_count"] == 0
                if not receipt["descendants_complete"]:
                    receipt.update(reason="descendants-still-running", returncode=receipt["returncode"] or 125)
                break
            remaining = None if timeout is None else timeout - (time.monotonic() - started)
            if remaining is not None and remaining <= 0:
                receipt["reason"] = "timeout"
                break
            time.sleep(INTERVAL if remaining is None else min(INTERVAL, remaining))
    except BlockingIOError:
        receipt.update(reason="guard-busy", error="another Cargo memory watchdog is active for this user")
    except (Exception, KeyboardInterrupt) as error:
        receipt.update(reason="measurement-error", error=f"{type(error).__name__}: {error}", returncode=125)
    finally:
        observer_stop.set()
        try:
            if process is not None:
                receipt["forced_cleanup"] = not receipt["descendants_complete"]
                for number in handlers:
                    signal.signal(number, signal.SIG_IGN)
                try:
                    stop_group(process)
                    receipt["cleanup_complete"] = True
                    receipt["command_returncode"] = process.returncode
                except Exception as error:
                    receipt.update(reason="cleanup-error", error=f"{type(error).__name__}: {error}", returncode=125)
        finally:
            if observer is not None:
                observer.join()
                receipt["observer_complete"] = True
                if observer_errors:
                    receipt["observation_error"] = observer_errors[0]
                    if receipt["reason"] == "command-exited":
                        receipt.update(reason="observation-error", error=observer_errors[0], returncode=125)
            if lock is not None:
                os.close(lock)
            for number, handler in handlers.items():
                signal.signal(number, handler)
            receipt["elapsed_seconds"] = time.monotonic() - started
    return receipt


def pinned_environment(environment, *, clippy=False):
    """Select absolute Rust tools, one build job and no compiler wrappers."""
    paths = {}
    tools = ["cargo", "rustc", "rustdoc"]
    if clippy:
        tools.extend(("cargo-clippy", "clippy-driver"))
    for tool in tools:
        value = subprocess.check_output(
            ["rustup", "which", "--toolchain", TOOLCHAIN, tool], text=True, timeout=10,
        ).strip()
        path = Path(value)
        if not path.is_absolute() or not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"rustup did not return an executable absolute path for {tool}")
        paths[tool] = str(path)
    directory = str(Path(paths["cargo"]).parent)
    if any(str(Path(path).parent) != directory for path in paths.values()):
        raise ValueError("the pinned Rust tools are in different directories")
    env = dict(environment)
    env.update(CARGO=paths["cargo"], RUSTC=paths["rustc"], RUSTDOC=paths["rustdoc"],
               RUSTUP_TOOLCHAIN=TOOLCHAIN, RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="",
               CARGO_BUILD_RUSTC_WRAPPER="", CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER="",
               CARGO_BUILD_JOBS="1", PATH=directory + os.pathsep + env.get("PATH", ""))
    return env


def cargo_command(arguments, environment):
    """Pin nested Rust tools and prevent command-line overrides of the build guard."""
    if not arguments or arguments[0] not in BUILD_COMMANDS:
        raise ValueError("start with a supported Cargo build command, without an alias or toolchain override")
    for argument in arguments:
        if argument == "--":
            break
        short_options = argument[1:] if argument.startswith("-") and not argument.startswith("--") else ""
        if argument.startswith(("-j", "--jobs", "--config")) or short_options.lstrip("qvrhV").startswith("j"):
            raise ValueError("Cargo job and configuration overrides are not allowed by the memory guard")
    env = pinned_environment(environment, clippy=arguments[0] == "clippy")
    separator = arguments.index("--") if "--" in arguments else len(arguments)
    guarded = [*arguments[:separator], "--jobs=1", *arguments[separator:]]
    return [env["CARGO"], *guarded], env


def qualification_command(arguments, environment):
    """Dispatch only the fixed canonical script with its three original arguments."""
    if len(arguments) != 3 or any(not value or value.startswith("-") or "\0" in value for value in arguments):
        raise ValueError("canonical qualification requires candidate JSON, reference worktree or revision, and reference JSON")
    script = Path(__file__).resolve().with_name("qualify-performance.sh")
    if not script.is_file():
        raise ValueError("the canonical qualification script is missing")
    return ["/bin/bash", str(script), *arguments], pinned_environment(environment)


def bind_qualification_outputs(receipt, arguments, receipt_path, script_sha256):
    """Bind both completed files only after the owned group has exited naturally."""
    if (receipt.get("started") is not True or receipt.get("reason") != "command-exited"
            or any(type(receipt.get(key)) is not int or receipt[key] != 0
                   for key in ("returncode", "command_returncode"))
            or any(receipt.get(key) is not True
                   for key in ("cleanup_complete", "descendants_complete"))
            or receipt.get("forced_cleanup") is not False):
        raise ValueError("canonical qualification did not complete naturally under the memory watchdog")
    bindings = {}
    for role, argument in (("candidate", arguments[0]), ("reference", arguments[2])):
        path = Path(argument)
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"canonical {role} output is missing or linked")
        data = path.read_bytes()
        cohort = json.loads(data)["metadata"]["cohort"]
        if (cohort.get("memory_watchdog_receipt") != str(receipt_path)
                or cohort.get("memory_watchdog_sha256") != SOURCE_SHA256
                or cohort.get("qualification_script_sha256") != script_sha256):
            raise ValueError(f"canonical {role} output does not match the guarded capture")
        bindings[role] = {"path": str(path.resolve()), "sha256": hashlib.sha256(data).hexdigest()}
    receipt["qualification_outputs"] = bindings


def main():
    if sys.argv[1:] == ["--check-owned-group"]:
        try:
            require_guarded_group()
        except ValueError as error:
            print(error, file=sys.stderr)
            return 125
        return 0
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", type=Path, required=True, help="new JSON receipt path")
    parser.add_argument("--timeout", type=float, help="maximum seconds; Cargo default: 3600; required for qualification")
    parser.add_argument("--qualify-performance", action="store_true", help="run only the canonical qualification script")
    parser.add_argument("cargo_arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.qualify_performance and args.timeout is None:
        parser.error("--qualify-performance requires an explicit --timeout")
    if args.timeout is not None and (not math.isfinite(args.timeout) or args.timeout <= 0):
        parser.error("--timeout must be finite and positive")
    timeout = 3600 if args.timeout is None else args.timeout
    arguments = args.cargo_arguments
    if arguments[:1] == ["--"]:
        arguments = arguments[1:]
    # Reserve the receipt before starting work. Never overwrite earlier evidence.
    with args.receipt.open("x", encoding="utf-8") as stream:
        receipt = None
        try:
            prepare = qualification_command if args.qualify_performance else cargo_command
            command, env = prepare(arguments, os.environ)
            if args.qualify_performance:
                receipt_path = args.receipt.resolve()
                env["RCA_MAC_MEMORY_GUARD_RECEIPT"] = str(receipt_path)
                script_sha256 = hashlib.sha256(Path(command[1]).read_bytes()).hexdigest()
            receipt = run_guarded(command, cwd=Path.cwd(), env=env, probe=MacProbe(), timeout=timeout)
            receipt["toolchain"] = TOOLCHAIN
            if args.qualify_performance:
                receipt.update(entrypoint="qualify-performance", qualification_script_sha256=script_sha256)
                if receipt["returncode"] == 0:
                    bind_qualification_outputs(receipt, arguments, receipt_path, script_sha256)
        except Exception as error:
            if receipt is None:
                receipt = {"started": False, "returncode": 125, "reason": "setup-error", "error": str(error)}
            else:
                receipt.update(returncode=125, reason="qualification-output-error", error=str(error))
        json.dump(receipt, stream, indent=2, allow_nan=False)
        stream.write("\n")
    print(f"Cargo memory watchdog: {receipt['reason']}. Receipt: {args.receipt}", file=sys.stderr)
    return receipt["returncode"] if receipt["returncode"] >= 0 else 128 - receipt["returncode"]


if __name__ == "__main__":
    sys.exit(main())
