"""Select a locally proven Rust compiler cache before the memory guard starts Cargo."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import tempfile

try:
    import tomllib
except ModuleNotFoundError:  # Cache selection requires a TOML parser.
    tomllib = None


KACHE_VERSION = "kache 0.26.3"
POLICY_SHA256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()


def digest(path):
    """Hash a file without loading the whole executable into memory."""
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def host_identity():
    host = os.uname()
    return [host.sysname, host.release, host.machine]


def proof_path(command):
    """Use a per-user path outside the checkout, independent of the Cargo target."""
    return Path.home() / "Library/Caches/rust-composable-architecture" / f"kache-{command}-proof.json"


def standard_cache_settings(env, cwd):
    """Skip automatic caching when it would replace a developer's build or cache settings."""
    native = {"CC": ("/usr/bin/clang", "kache cc"), "CXX": ("/usr/bin/clang++", "kache c++"),
              "HOST_CC": ("/usr/bin/clang", "kache cc"), "HOST_CXX": ("/usr/bin/clang++", "kache c++"),
              "CC_KNOWN_WRAPPER_CUSTOM": ("kache",)}
    if any(env.get(key) and env[key] not in allowed for key, allowed in native.items()):
        return False
    if (env.get("KACHE_DISABLED", "").lower() in ("1", "true") or any(value for key, value in env.items()
            if key.startswith("KACHE_") and key not in ("KACHE_CACHE_DIR", "KACHE_DISABLED"))):
        return False

    # Do not interpret project, user or machine kache policies for the private wrapper.
    ancestors = (cwd, *cwd.parents)
    user_config = Path(env.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    if (Path("/etc/kache/config.toml").exists() or (user_config / "kache/config.toml").exists()
            or any((directory / ".kache.toml").exists() for directory in ancestors)):
        return False

    # Accept only the ordinary native tool settings. Cargo handles all other
    # configuration itself on the original guard path, including config includes.
    cargo_dirs = {directory / ".cargo" for directory in ancestors}
    cargo_dirs.add(Path(env.get("CARGO_HOME", Path.home() / ".cargo")))
    for directory in cargo_dirs:
        for name in ("config", "config.toml"):
            path = directory / name
            if not path.exists():
                continue
            if not path.is_file() or path.stat().st_size > 65536:
                return False
            with path.open("rb") as source:
                config = tomllib.load(source)
            native_env = config.get("env", {})
            if (config.keys() - {"env", "build"} or config.get("build", {}) not in ({}, {"rustc-wrapper": "kache"})
                    or not isinstance(native_env, dict)
                    or any(value not in native.get(key, ()) for key, value in native_env.items())):
                return False
    return True


def kache_environment(environment, binary, scratch, store, expected_hash):
    """Pin the wrapper for this run and keep cache work local to this machine."""
    if environment.get("KACHE_TRUST_DOMAIN"):
        raise ValueError("cannot use kache with a namespaced store")
    if (environment.get("CARGO_BUILD_TARGET") or any(value for key, value in environment.items()
            if key in ("TARGET_CC", "TARGET_CXX") or (key.startswith(("CC_", "CXX_"))
            and key != "CC_KNOWN_WRAPPER_CUSTOM"))):
        raise ValueError("cannot use kache with a cross target or custom target compiler")
    wrapper = scratch / "kache"
    shutil.copy2(binary, wrapper)
    if digest(wrapper) != expected_hash:
        raise ValueError("kache executable changed; repeat the proof")
    config = scratch / "config.toml"
    config.write_text(
        '[cache]\nignore_env=true\nlocal_only=true\nauto_gc=false\n'
        'local_hit_daemon=false\ndeferred_durability=false\nscheduler=false\n'
        f'local_store={json.dumps(str(store))}\nruntime_dir={json.dumps(str(scratch / "run"))}\n',
        encoding="utf-8",
    )
    env = {k: v for k, v in environment.items() if not k.startswith("KACHE_")}
    # Only Rust compilation was qualified. Host C and C++ must not use kache shims.
    env.update(KACHE_CONFIG=str(config), KACHE_HOST_CONFIG="", KACHE_CACHE_DIR=str(store),
               KACHE_SOCKET_PATH=str(scratch / "private.sock"), KACHE_BUILD_SCRIPT_CACHE="0",
               RUSTC_WRAPPER=str(wrapper), CARGO_BUILD_RUSTC_WRAPPER=str(wrapper), CARGO_INCREMENTAL="0",
               HOST_CC="/usr/bin/clang", HOST_CXX="/usr/bin/clang++",
               CC="/usr/bin/clang", CXX="/usr/bin/clang++",
               PATH=os.pathsep.join((str(Path(env["CARGO"]).parent), "/usr/bin", env["PATH"])))
    return env


def checked_proof(path, env, command, guard_sha256):
    """Reject incomplete proof and changes to the host, guard, policy or compiler."""
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise ValueError("proof must be a user-owned file that others cannot change")
        raw = stream.read(65537)
        if len(raw) > 65536:
            raise ValueError("proof is too large")
        data = json.loads(raw)
    abort, retry, warm = (data[k] for k in ("abort", "retry", "warm"))
    sample = abort["threshold_sample"]
    miss, hit = retry["events"], warm["events"]
    if not (command in ("check", "build") and data["cargo_command"] == command
            and data["outcome"] == "pass" and data["version"] == KACHE_VERSION
            and data["guard_sha256"] == guard_sha256 and data["policy_sha256"] == POLICY_SHA256
            and data["host"] == host_identity() and data["rustc_sha256"] == digest(Path(env["RUSTC"]))
            and len(bytes.fromhex(data["artifact_sha256"])) == 32 and data["existing_daemons_unchanged"] >= 0
            and abort["started"] is True and abort["reason"] == "memory-limit"
            and abort["forced_cleanup"] is True and abort["cleanup_complete"] is True and abort["live_after"] == 0
            and sample["pgid"] == abort["pid"] and sample["rustc_pid"] > 0 and sample["kache_pid"] > 0
            and 0 < sample["compiler_bytes"] <= sample["total"]
            and sample["total"] >= abort["memory_limit_bytes"] > sample["total"] - sample["compiler_bytes"]
            and all(r["cleanup_complete"] is True and r["reason"] == "command-exited"
                    and r["returncode"] == 0 and r["forced_cleanup"] is False for r in (retry, warm))
            and len(miss) == len(hit) == 1 and miss[0]["result"] == "miss" and miss[0]["compiler_runs"] == 1
            and hit[0]["result"] == "local_hit" and hit[0]["compiler_runs"] == 0
            and miss[0]["cache_key"] and miss[0]["cache_key"] == hit[0]["cache_key"]):
        raise ValueError("invalid or stale kache proof; repeat the proof")
    installed = shutil.which("kache", path=env["PATH"])
    if not installed or Path(installed).resolve() != Path(data["kache"]):
        raise ValueError("kache on PATH differs from the proven executable")
    return Path(installed).resolve(strict=True), data["kache_sha256"]


def select_cache(arguments, env, guard_sha256, stack):
    """Fall back only during cache setup. The caller runs the command exactly once."""
    off = {"rust_compiler_cache": "off"}
    if (tomllib is None or arguments[:1] not in (["build"], ["check"])
            or any(arg == "--target" or arg.startswith("--target=") for arg in arguments)
            or env.get("CARGO_INCREMENTAL") == "1"
            or not shutil.which("kache", path=env["PATH"])):
        return env, off
    try:
        # External benchmark manifests must not inherit an RCA checkout's cache policy.
        checkout = Path(__file__).resolve().parents[1]
        if not Path.cwd().is_relative_to(checkout):
            return env, off
        for index, arg in enumerate(arguments):
            if arg == "--manifest-path" or arg.startswith("--manifest-path="):
                manifest = arguments[index + 1] if arg == "--manifest-path" else arg.partition("=")[2]
                if not Path(manifest).resolve().is_relative_to(checkout):
                    return env, off
        if not standard_cache_settings(env, Path.cwd()):
            return env, off
        path = proof_path(arguments[0])
        binary, binary_hash = checked_proof(path, env, arguments[0], guard_sha256)
        store = Path(env.get("KACHE_CACHE_DIR", Path.home() / "Library/Caches/kache"))
        if not store.is_absolute() or not (store / "index.db").is_file():
            raise ValueError("local kache store unavailable")
        scratch = stack.enter_context(tempfile.TemporaryDirectory(prefix="rca-k-", dir="/private/tmp"))
        selected = kache_environment(env, binary, Path(scratch), store, binary_hash)
    except (OSError, ValueError, TypeError, LookupError, RecursionError) as error:
        return env, {**off, "cache_setup_error": str(error)}
    return selected, {"rust_compiler_cache": "kache", "kache_sha256": binary_hash,
                      "policy_sha256": POLICY_SHA256}
