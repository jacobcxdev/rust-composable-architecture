"""Build composition benchmarks and candidate Core diagnostics during guarded preparation only."""

import hashlib
import json

import composition_performance as evidence
import composition_registry_seed as registry
from composition_runner import source_hashes


CORE_ROWS = {
    "effects": {
        "action_task_cancel", "far_timer_removal", "immediate_send_round_trip",
        "overdue_interval_catch_up", "run_effect_round_trip",
        *(f"generation_cancellation/{n}" for n in (1, 16, 256)),
        *(f"zero_deadline_timers/{n}" for n in (1, 16, 256)),
    },
    "observation": {
        "cancelled_next_resume", "large_projection_publication", "observation_drop_cleanup",
        "observation_handoff", "slow_consumer_slot_replacement",
        *(f"active_observer_publication/{n}" for n in (0, 1, 8, 64)),
        *(f"shutdown_with_observers/{n}" for n in (1, 8, 64)),
    },
    "store_actor": {f"effect_free_commits/{n}" for n in (1, 2, 4, 8)},
}
COMPOSITION_LIST = "6ab7c20f80fa308d67553fc6ddc1040cc1847988869571a22c2aaa855e60387b"
PROFILE = {"opt_level": "3", "debuginfo": 0, "test": True, "debug_assertions": False, "overflow_checks": False}
BUILD_SETTINGS = {"CARGO_PROFILE_BENCH_DEBUG": "false", "CARGO_PROFILE_BENCH_LTO": "false",
                  "CARGO_PROFILE_BENCH_STRIP": "true"}


def source_digest(source):
    """Identify the complete staged source, including the common workload overlay."""
    data = json.dumps(source_hashes(source), sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(data).hexdigest()


def benchmark_paths(log, source, target, names):
    """Read one fresh executable per expected benchmark from the successful Cargo transcript."""
    paths = {}
    for row in registry._records(log):
        if row.get("reason") != "compiler-artifact" or row["target"]["kind"] != ["bench"]:
            continue
        name = row["target"]["name"]
        if (name not in names or name in paths or row["fresh"] is not False or not row["executable"]
                or row["executable"] not in row["filenames"] or not row["package_id"].startswith("path+")
                or row["profile"] != PROFILE or row["features"] != ["default"]
                or type(target)(row["manifest_path"]) != source / "rca-core/Cargo.toml"
                or type(target)(row["target"]["src_path"]) != source / f"rca-core/benches/{name}.rs"):
            raise ValueError(f"wrong or duplicate Core benchmark build: {name}")
        path = type(target)(row["executable"])
        if not path.is_relative_to(target) or ".." in path.parts or path in paths.values():
            raise ValueError("Core benchmark outputs must be distinct and inside their own target")
        paths[name] = path
    if set(paths) != set(names):
        raise ValueError("Cargo did not compile every expected Core benchmark")
    return paths


def read_diagnostic(name, transcript):
    """Check the full frozen composition listing or the exact Core suite rows."""
    if name == "composition":
        lines = transcript.read_text(encoding="utf-8").splitlines()
        digest = hashlib.sha256(("\n".join(lines) + "\n").encode()).hexdigest()
        if len(lines) != 68 or digest != COMPOSITION_LIST:
            raise ValueError("composition benchmark list changed")
        return {"list_sha256": digest}
    rows = evidence.parse_divan_transcript(transcript)
    if rows.keys() != CORE_ROWS[name]:
        raise ValueError(f"missing or unexpected diagnostic rows: {name}")
    return {"rows": rows}


def _check_monitor(monitor, tools):
    if (getattr(monitor, "complete", False) is not True or getattr(monitor, "cargo", None) != tools["cargo"]
            or monitor.protocol.get("process_memory_limit_bytes") != 2 * 1024**3):
        raise ValueError("benchmark preparation requires the complete 2 GiB Windows Job monitor and pinned Cargo")


def _check_env(env):
    for key, value in env.items():
        if key.startswith("CARGO_PROFILE_") and BUILD_SETTINGS.get(key) != value:
            raise ValueError("unexpected benchmark profile override")
        if key.startswith("DIVAN_") or (key in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS") and value):
            raise ValueError("unexpected workload or compiler override")


def prepare(source, target, staging, tools, env, monitor, *, seed, lock_sha256, effective_sha256):
    """Use one guarded Cargo command per role; only the candidate also produces Core diagnostics.

    Use the controller's pinned tool environment and Windows monitor. Supply the staged
    source with its lock and effective-source hashes. For the cold donor, pass no seed.
    Its unchanged build log can supply the existing registry seeder.
    This helper is not a performance verdict and never handles a timed cold target.
    """
    _check_monitor(monitor, tools)
    mode, role = target.parent.name, target.name
    log = staging / "builds" / (role + ".txt")
    if (mode not in ("candidate-reference", "reference-reference-aa") or role not in ("control", "subject")
            or source != target.parent / ("source-" + role) or staging.name != mode
            or any(p.resolve() != p for p in (source, target, staging, log))
            or target.exists() or log.exists() or staging.is_relative_to(source) or staging.is_relative_to(target)
            or (seed is not None and (mode, role) == ("candidate-reference", "control"))):
        raise ValueError("expected a new, isolated normal benchmark preparation target")
    if evidence._sha256(source / "Cargo.lock") != lock_sha256 or source_digest(source) != effective_sha256:
        raise ValueError("prepared source or lock differs from the enrolled inputs")
    _check_env(env)
    names = ("composition", *CORE_ROWS) if (mode, role) == ("candidate-reference", "subject") else ("composition",)
    allowed = registry.seed_benchmark(seed, target, lock_sha256=lock_sha256) if seed is not None else {}
    options = {**env, **BUILD_SETTINGS, "CARGO_TARGET_DIR": str(target), "CARGO_BUILD_BUILD_DIR": str(target)}
    command = [tools["cargo"], "bench", "--locked", "-p", "rca-core"]
    for name in names:
        command.extend(["--bench", name])
    command.extend(["-j", "2", "--no-run", "--message-format=json"])
    log.parent.mkdir(parents=True, exist_ok=True)
    monitor.run(command, log, source, options)
    registry.verify_rebuild(log, allowed)
    paths = benchmark_paths(log, source, target, names)
    digests = {name: evidence._sha256(registry._regular(path)) for name, path in paths.items()}
    results = {}
    for name in names:
        path = paths[name]
        args = (["--list"] if name == "composition" else
                ["--bench", "--color", "never", "--sample-count", "50", "--max-time", "0.5"])
        transcript = staging / "builds" / (role + "-" + name + ".txt")
        if transcript.exists() or transcript.is_symlink():
            raise ValueError("benchmark transcript already exists")
        if evidence._sha256(registry._regular(path)) != digests[name]:
            raise ValueError("benchmark executable changed before its diagnostic")
        monitor.run([str(path), *args], transcript, source, options)
        results[name] = {"path": str(path), "sha256": digests[name], "transcript_sha256": evidence._sha256(transcript),
                         **read_diagnostic(name, transcript)}
    if (source_digest(source) != effective_sha256
            or any(evidence._sha256(registry._regular(p)) != digests[n] for n, p in paths.items())):
        raise ValueError("benchmark source or executable changed during preparation")
    return {"build_sha256": evidence._sha256(log), "executables": results}


def _fixed_images(cache, enrolment_sha256, inputs):
    from qualification_reference_cache import verify_reference_products

    pair = verify_reference_products(cache, enrolment_sha256=enrolment_sha256, inputs=inputs,
                                     purpose="composition-normal", names=("composition",), roles=("a", "b"))
    return {role: registry._regular(pair[role]["composition"]) for role in ("a", "b")}


def list_fixed_references(cache, staging, tools, env, monitor, *, enrolment_sha256, reference_inputs):
    """Verify both independent reference images, then run and check each full workload listing.

    Supply the expected inputs and enrolment digest from outside the cache. Run inside the host memory guard.
    This does not measure timing, qualify the candidate or supply a cold build.
    """
    _check_monitor(monitor, tools)
    _check_env(env)
    if (staging.is_relative_to(cache) or staging.name != "candidate-reference"
            or (staging / "builds").resolve() != staging / "builds"):
        raise ValueError("reference listings require a separate candidate-reference evidence directory")
    images = _fixed_images(cache, enrolment_sha256, reference_inputs)
    roles = {}
    for role, image in images.items():
        transcript = staging / "builds" / f"reference-{role}-composition.txt"
        if transcript.exists() or transcript.is_symlink():
            raise ValueError("reference listing already exists")
        transcript.parent.mkdir(parents=True, exist_ok=True)
        digest = evidence._sha256(image)
        monitor.run([str(image), "--list"], transcript, staging, env)
        roles[role] = {"composition": {"path": str(image), "sha256": digest,
                                      "transcript_sha256": evidence._sha256(transcript),
                                      **read_diagnostic("composition", transcript)}}
    if any(evidence._sha256(p) != roles[r]["composition"]["sha256"] for r, p in images.items()):
        raise ValueError("fixed reference executable changed during its listing")
    return roles


def prepare_with_fixed_references(source, target, staging, tools, env, monitor, *, cache, enrolment_sha256,
                                  reference_inputs, lock_sha256, effective_sha256):
    """List both fixed references and build all candidate products in a new, unseeded target."""
    if (target.parent.name != "candidate-reference" or target.name != "subject"
            or source != target.parent / "source-subject" or target.exists() or target.is_relative_to(cache)
            or staging.is_relative_to(source) or staging.is_relative_to(target)
            or any(p.resolve() != p for p in (source, target))):
        raise ValueError("fixed references require a separate fresh candidate preparation target")
    roles = list_fixed_references(cache, staging, tools, env, monitor, enrolment_sha256=enrolment_sha256,
                                  reference_inputs=reference_inputs)
    candidate = prepare(source, target, staging, tools, env, monitor, seed=None,
                        lock_sha256=lock_sha256, effective_sha256=effective_sha256)
    images = _fixed_images(cache, enrolment_sha256, reference_inputs)
    if any(str(p) != roles[r]["composition"]["path"] or
           evidence._sha256(p) != roles[r]["composition"]["sha256"] for r, p in images.items()):
        raise ValueError("fixed reference products changed during preparation")
    roles["c"] = candidate["executables"]
    return {"reference_enrolment_sha256": enrolment_sha256, "roles": roles,
            "candidate_build_sha256": candidate["build_sha256"]}
