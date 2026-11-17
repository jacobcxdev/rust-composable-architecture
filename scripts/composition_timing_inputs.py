"""Read real, sealed Windows v6 timing prefixes from a trusted capture producer.

The caller supplies the ledger's previously sealed digests, not freshly calculated
hashes of a purported result. This verifies measurements, not release qualification.
"""

import json
import math
import os
from pathlib import Path, PureWindowsPath
import stat

import composition_performance as evidence


MODES = ("reference-reference-aa", "candidate-reference")


def _jobs(protocol, counts):
    workloads = [("runtime", s) for s in protocol["runtime"]["gated_scenarios"]]
    workloads += [("compile", s) for s in protocol["compile"]["fixtures"]]
    if (protocol.get("method_version") != evidence.WINDOWS_V6_METHOD
            or set(counts) != {f"{kind}/{s}" for kind, s in workloads}
            or any(type(n) is not int or not 1 <= n <= 20 for n in counts.values())):
        raise ValueError("timing prefixes require the complete Windows v6 workload set and counts 1..20")
    jobs = []
    for n, order in enumerate(evidence.seeded_order_schedule(protocol), 1):
        for mode in evidence.interleaved_mode_order(protocol, n):
            for kind, s in workloads:
                key = f"{kind}/{s}"
                if n <= counts[key]:
                    for role in order.split("-"):
                        rel = f"{mode}/paired/{role}/{kind}/{s.replace('/', '__')}/block-{n:02d}.txt"
                        jobs.append((n, mode, role, key, rel))
    return jobs


def _verify_seals(directory, jobs, seals):
    if set(seals) != set(MODES):
        raise ValueError("timing seals require both modes")
    for mode in MODES:
        transcripts = [Path(rel) for _, m, _, _, rel in jobs if m == mode]
        expected = {p.as_posix() for path in transcripts
                    for p in (path, path.with_suffix(".resources.json"))}
        root = directory / mode / "paired"
        if any(p.is_symlink() for p in (directory, root.parent, root)):
            raise ValueError("redirected timing evidence")
        files = set()
        for path in root.rglob("*"):
            if path.is_symlink() or (hasattr(path, "is_junction") and path.is_junction()):
                raise ValueError("redirected timing evidence")
            if path.is_file():
                files.add(path.relative_to(directory).as_posix())
        if files != expected or set(seals[mode]) != expected:
            raise ValueError(f"missing or extra sealed timing inputs: {mode}")
        for rel in expected:
            if evidence._sha256(directory / rel) != seals[mode][rel]:
                raise ValueError(f"timing input changed after sealing: {rel}")


def _json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def _executable(build):
    executables = []
    for line in build.read_text(encoding="utf-8").splitlines():
        if line.startswith("{"):
            item = json.loads(line)
            if (item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "composition"
                    and item.get("executable")):
                executables.append(PureWindowsPath(item["executable"]))
    if len(executables) != 1 or not executables[0].is_absolute():
        raise ValueError("missing unique composition runtime build product")
    return executables[0]


def _verify_tools(inputs, meta):
    """Check the controller script and tool identities against the manifest-bound input list."""
    hashes = inputs["input_hashes"]
    root = PureWindowsPath(inputs["controller"]["root"])
    for field, script in (("qualification_script_sha256", "composition_runner.py"),
                          ("evaluator_script_sha256", "composition_performance.py")):
        digest = hashes.get(str(root / "scripts" / script))
        if not evidence._HASH.fullmatch(str(digest)) or meta.get(field) != digest:
            raise ValueError("timing controller script identity differs")
    capture = inputs["controller"]["hashes"]["scripts/capture-composition-performance.sh"]
    if not evidence._HASH.fullmatch(str(capture)) or meta.get("capture_script_sha256") != capture:
        raise ValueError("timing capture script identity differs")
    for tool in inputs["tools"].values():
        digest = hashes.get(tool["path"])
        if not evidence._HASH.fullmatch(str(digest)) or tool.get("sha256") != digest:
            raise ValueError("timing tool identity differs")


def _identities(directory, protocol):
    manifest = _json(directory / "capture.json")
    digest = evidence._sha256(directory / "capture.json")
    experimental = "experiment" in manifest
    if experimental:
        from composition_runner import rank_method_identity
        if manifest["experiment"] != rank_method_identity():
            raise ValueError("missing or changed prospective method identity")
    schedule = [{"block": n, "mode": m} for n in range(1, 21)
                for m in evidence.interleaved_mode_order(protocol, n)]
    if (manifest.get("protocol_sha256") != evidence.WINDOWS_V6_PROTOCOL_SHA256
            or set(manifest.get("modes", {})) != set(MODES) or manifest.get("schedule") != schedule):
        raise ValueError("capture has a different protocol, schedule or mode set")
    result = {}
    frozen_reference = None
    shared_tools = None
    for mode in MODES:
        arm = directory / mode
        if evidence.load_protocol(arm / "protocol.json") != protocol:
            raise ValueError("changed timing protocol")
        bindings = {name: evidence._sha256(arm / name) for name in ("plan.json", "inputs.json")}
        inputs, meta = _json(arm / "inputs.json"), _json(arm / "metadata.json")
        evidence._validate_measurement_plan(arm / "plan.json", protocol, mode)
        evidence._validate_protocol_pair_metadata(meta, protocol["method_version"])
        host = manifest["host"]["identity"]
        if (bindings != manifest["modes"][mode] or inputs.get("mode") != mode
                or inputs.get("controller") != manifest["controller"] or inputs.get("host") != manifest["host"]
                or meta.get("capture_sha256") != digest or meta.get("plan_sha256") != bindings["plan.json"]
                or meta.get("mode") != mode or meta.get("runner") != protocol["runner"]
                or meta.get("method_version") != protocol["method_version"]
                or meta.get("protocol_sha256") != manifest["protocol_sha256"]
                or meta.get("budget_sha256") != evidence.BUDGET_SHA256
                or meta.get("toolchain") != evidence.expected_toolchain(protocol)
                or any(meta.get(k) != host[k] for k in ("os_build", "os_version", "target", "platform"))):
            raise ValueError("detached timing capture, mode, tools or host")
        evidence.validate_protocol_sources(arm, meta, protocol)
        _verify_tools(inputs, meta)
        tools = inputs["tools"]
        if shared_tools is not None and tools != shared_tools:
            raise ValueError("timing modes used different tools")
        shared_tools = tools
        if mode == "candidate-reference" and inputs["subject"] != manifest["controller"]:
            raise ValueError("candidate inputs differ from controller")
        for role in ("subject", "control"):
            source = _json(arm / "sources" / f"{role}.json")
            recorded = inputs[role]
            if (source["revision"] != recorded["revision"] or source["tree"] != recorded["tree"]
                    or meta[f"{role}_lock_sha256"] != recorded["hashes"]["Cargo.lock"]
                    or source["effective"]["Cargo.lock"] != recorded["hashes"]["Cargo.lock"]
                    or source["effective"]["benches/compile/Cargo.lock"] != recorded["hashes"]["benches/compile/Cargo.lock"]):
                raise ValueError("source tree or lock differs from timing inputs")
            if role == "control" or mode == "reference-reference-aa":
                identity = (source["tree"], source["original"], source["effective"])
                if frozen_reference is not None and identity != frozen_reference:
                    raise ValueError("three independently staged reference arms differ")
                frozen_reference = identity
            cwd = PureWindowsPath(source["root"])
            executable = _executable(arm / "builds" / f"{role}.txt")
            if (not cwd.is_absolute() or not executable.is_relative_to(cwd.parent / role)
                    or any(cwd == old[0] or executable == old[2] for old in result.values())):
                raise ValueError("source or runtime product is not independent by role")
            result[mode, role] = (cwd, PureWindowsPath(tools["cargo"]["path"]), executable)
    return result, experimental


def _verify_processes(directory, jobs, identities, protocol, *, experimental_rank=False):
    # The stored absolute destinations may name a Windows staging path even when
    # the evidence is now being reviewed from a published directory on macOS.
    expected = ((phase, job) for job in jobs for phase in ("start", "finish"))
    roots, targets, lifetimes = set(), set(), set()
    last_time = -1
    with (directory / "telemetry/processes.jsonl").open(encoding="utf-8") as stream:
        for line in stream:
            event = json.loads(line)
            dst = PureWindowsPath(event["destination"])
            if "paired" not in dst.parts:
                continue
            phase, job = next(expected, (None, None))
            if phase != event.get("phase"):
                raise ValueError("timed process history is incomplete or reordered")
            n, mode, role, work, rel = job
            path = PureWindowsPath(rel)
            if not dst.is_absolute() or dst.parts[-len(path.parts):] != path.parts:
                raise ValueError("timed process belongs to a different block, role or workload")
            roots.add(dst.parents[len(path.parts)-1])
            instant = event["monotonic"]
            if (len(roots) != 1 or type(instant) not in (int, float) or not math.isfinite(instant)
                    or instant < last_time or event.get("job_memory_limit_bytes") != protocol["process_memory_limit_bytes"]):
                raise ValueError("timed process belongs to another capture or is out of order")
            last_time = instant
            if phase == "finish":
                resources = _json((directory/rel).with_suffix(".resources.json"))
                if event.get("returncode") != 0 or event.get("process_receipt") != resources["process_receipt"]:
                    raise ValueError("timed process finish differs from sealed receipt")
                counters = resources["direct_process"]
                lifetime = (counters["CreationTime100ns"], counters["ExitTime100ns"])
                if lifetime in lifetimes:
                    raise ValueError("timed process measurement was reused")
                lifetimes.add(lifetime)
                continue
            kind, name = work.split("/", 1)
            cwd, cargo, executable = identities[mode, role]
            cmd = event["command"]
            if (event.get("command_kind") != kind or PureWindowsPath(event["cwd"]) != cwd
                    or event.get("timeout_seconds") != protocol["process_timeout_seconds"]):
                raise ValueError("timed process command is detached from source or policy")
            if kind == "compile":
                target = PureWindowsPath(event["target"])
                if (target != cwd.parent / f"compile-{name}-{n:02d}-{role}" or target in targets
                        or event.get("incremental") != "0" or event.get("compiler_telemetry") != protocol["compiler_telemetry"]
                        or cmd != [str(cargo), "check", "--manifest-path", str(cwd / "benches/compile/Cargo.toml"),
                                   "-p", f"rca-compile-{name}", "--locked", "-j", "1"]):
                    raise ValueError("cold compile target, command or cleanup identity differs")
                targets.add(target)
            elif (event.get("target") is not None or event.get("compiler_telemetry") is not None
                  or cmd != [str(executable), "--bench", "--exact", "composition::"+name.replace("/", "::"),
                             *evidence.isolated_divan_arguments(protocol, name, experimental_rank=experimental_rank)]):
                raise ValueError("runtime image or command differs from frozen cell")
    if next(expected, None) is not None:
        raise ValueError("missing timed process records")


def load_timing_prefixes(directory, protocol, budget_path, counts_by_workload, seals):
    """Return all fourteen budget rows with real A/A and candidate pairs, grouped by workload."""
    directory = Path(directory)
    jobs = _jobs(protocol, counts_by_workload)
    identities, experimental = _identities(directory, protocol)
    _verify_seals(directory, jobs, seals)
    _verify_processes(directory, jobs, identities, protocol, experimental_rank=experimental)
    budget = evidence.validate_budget_document(Path(budget_path))
    groups = {w: [] for w in counts_by_workload}
    for row in budget["rows"]:
        work = row["scenario"].removesuffix("/clean")
        if work in groups:
            groups[work].append(row)
    result = {}
    for work, rows in groups.items():
        kind, name = work.split("/", 1)
        load = evidence.load_compile_pairs if kind == "compile" else evidence.load_isolated_runtime_pairs
        both = []
        for mode in MODES:
            paired = directory / mode / "paired"
            both.append(load(paired/"subject", paired/"control", name, protocol, block_count=counts_by_workload[work],
                             **({"experimental_rank": experimental} if kind == "runtime" else {})))
        result[work] = []
        for row in rows:
            arms = both if kind == "compile" else [
                [{**p, **{r: evidence._runtime_value(row, {name: p[r]}) for r in ("subject", "control")}}
                 for p in pairs] for pairs in both]
            result[work].append((row, *arms))
    return result


def _reject_redirects(directory):
    """Reject links and Windows reparse points before reading any terminal evidence."""
    def check(path):
        state = path.lstat()
        if stat.S_ISLNK(state.st_mode) or (
                getattr(state, "st_file_attributes", 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT):
            raise ValueError(f"redirected terminal evidence: {path}")
    check(directory)
    def failed(error):
        raise error
    for root, dirs, files in os.walk(directory, onerror=failed, followlinks=False):
        for name in (*dirs, *files):
            check(Path(root) / name)


def _stamp(row, previous):
    instant = row.get("monotonic")
    if (type(instant) not in (int, float) or not math.isfinite(instant) or instant < previous
            or not evidence._TIMESTAMP.fullmatch(str(row.get("recorded_at", "")))):
        raise ValueError("terminal telemetry timestamp is missing or reordered")
    return instant


def _host_events(directory, blocks, host):
    path = directory / "telemetry/telemetry.jsonl"
    records = [json.loads(s) for s in path.read_text(encoding="utf-8").splitlines()]
    phases = ["initial", *[f"block-{n:02d}" for n in range(1, blocks+1) for _ in MODES], "final-host"]
    if [r.get("phase") for r in records] != phases:
        raise ValueError("missing or extra host admission or final host")
    times = []
    for row in records:
        times.append(_stamp(row, times[-1] if times else -1))
        expected = {"host":host} if row["phase"] == "final-host" else {"admission":"recorded-background-load"}
        if (any(row.get(k) != v for k, v in expected.items())
                or set(row) != {"phase","recorded_at","monotonic"} | expected.keys()):
            raise ValueError("changed host or invalid admission")
    return times


def _terminal_pair(directory, protocol, stage, spec, start, finish, allocations):
    """Bind one terminal command and its externally selected workload to its raw receipt."""
    from composition_windows_absolute import BINARY_NAMES
    rel, cmd, cwd, kind, target = spec
    dest = stage / rel
    path = directory / rel
    # The exact objcopy invocation is silent on success. Its successful job and
    # hash-bound output PE are the evidence; diagnostics and builds do produce text.
    silent_strip = cmd[1:2] == ["--strip-all"] and path.name in {f"strip-{name}.txt" for name in BINARY_NAMES}
    if (not path.is_file() or (not silent_strip and not path.stat().st_size) or start.get("phase") != "start"
            or finish.get("phase") != "finish" or start.get("command") != cmd
            or not {"incremental", "target", "compiler_telemetry"} <= start.keys() or "error" in finish
            or PureWindowsPath(start["cwd"]) != cwd or start.get("command_kind") != kind
            or start.get("target") != (str(target) if target is not None else None)
            or start.get("incremental") is not None
            or start.get("timeout_seconds") != protocol["process_timeout_seconds"]
            or start.get("compiler_telemetry") != (protocol["compiler_telemetry"] if kind == "compile" else None)
            or any(PureWindowsPath(r["destination"]) != dest
                   or r.get("job_memory_limit_bytes") != protocol["process_memory_limit_bytes"] for r in (start, finish))):
        raise ValueError(f"terminal command, location, target or policy differs: {rel}")
    allocation = path.stem if "/allocation/" in rel else None
    samples = str(allocations[allocation]["samples"]) if allocation is not None else None
    if start.get("allocation_scenario") != allocation or start.get("allocation_samples") != samples:
        raise ValueError(f"terminal allocation dispatch differs: {rel}")
    raw = _json(path.with_suffix(".resources.json"))
    divan = cmd[1:] == ["--bench"]
    evidence.validate_windows_resource_counters(raw, expected_kind=kind, primary_thread_affinity_required=divan)
    receipt = raw["process_receipt"]
    if ((receipt.get("divan_arguments") != (["--bench"] if divan else None))
            or type(finish.get("returncode")) is not int or finish["returncode"] != 0
            or finish.get("process_receipt") != receipt):
        raise ValueError(f"terminal process finish differs from raw receipt: {rel}")


def verify_terminal_processes(directory, protocol):
    """Verify the complete positive capture's process order, commands, resources and host.

    Call only after timing has passed and the collector has recorded final-host.
    The preceding timing verifier authenticates each paired command and its sealed files.
    """
    import composition_product_commands as products
    directory = Path(directory)
    _reject_redirects(directory)
    ids, _ = _identities(directory, protocol)
    counts = _json(directory / "experimental-timing.json")["counts"]
    jobs = _jobs(protocol, counts)
    host = _json(directory / "capture.json")["host"]
    host_times = _host_events(directory, max(counts.values()), host)
    rows = [json.loads(x) for x in (directory/"telemetry/processes.jsonl").read_text(encoding="utf-8").splitlines()]
    first_rel = PureWindowsPath(jobs[0][-1])
    first_events = [row for row in rows if row.get("phase") == "start"
                    and PureWindowsPath(row["destination"]).parts[-len(first_rel.parts):] == first_rel.parts]
    if len(first_events) != 1:
        raise ValueError("missing or duplicated staged timing path")
    stage = PureWindowsPath(first_events[0]["destination"]).parents[len(first_rel.parts)-1]
    if not stage.is_absolute() or ".." in stage.parts:
        raise ValueError("recorded Windows capture root is not absolute and normal")

    before, after = [], []
    for mode in reversed(MODES):
        for step in ("build", "list"):
            for role in ("control", "subject"):
                cwd, cargo, exe = ids[mode, role]
                target = cwd.parent / role if step == "build" else None
                cmd = ([str(cargo), "bench", "--locked", "-p", "rca-core", "--bench", "composition",
                        "-j", "1", "--no-run", "--message-format=json"] if step == "build" else [str(exe), "--list"])
                name = role if step == "build" else role+"-list"
                before.append((f"{mode}/builds/{name}.txt", cmd, cwd,
                               "compile" if step == "build" else "runtime", target))
    for mode in MODES:
        inputs, meta = (_json(directory/mode/f) for f in ("inputs.json", "metadata.json"))
        for role in ("control", "subject"):
            cwd, _, exe = ids[mode, role]
            label = meta[f"{role}_role"]
            after.append((f"{mode}/diagnostics/{label}.txt", [str(exe), "--bench"], cwd, "runtime", None))
        cwd, _, exe = ids[mode,"subject"]
        after.extend(products.product_commands(directory, mode, cwd, inputs["tools"], exe, stage))

    # Every telemetry event must belong to one actual command, including the
    # already sealed timing phase; no extra command, unmanaged outcome or retry.
    cells = [(spec[0], spec, None) for spec in before]
    cells.extend((rel, None, (n, mode)) for n,mode,_role,_key,rel in jobs)
    cells.extend((spec[0], spec, None) for spec in after)
    if len(rows) != 2 * len(cells):
        raise ValueError("missing or extra terminal process events")
    expected_raw = {Path(rel).with_suffix(".resources.json").as_posix() for rel,_,_ in cells}
    actual_raw = {p.relative_to(directory).as_posix() for p in directory.rglob("*.resources.json")}
    if actual_raw != expected_raw:
        raise ValueError("missing or extra process resource files")
    allocations = evidence.validate_allocation_manifest(evidence.DEFAULT_ALLOCATION_MANIFEST)
    lifetimes = set()
    previous = host_times[0]
    prior_group = None
    group_index = 0
    for index, (rel, spec, group) in enumerate(cells):
        start, finish = rows[2*index:2*index+2]
        if group is not None and group != prior_group:
            group_index += 1
            if host_times[group_index] < previous:
                raise ValueError("timing block admission precedes the previous command")
            previous = host_times[group_index]
            prior_group = group
        previous = _stamp(start, previous)
        previous = _stamp(finish, previous)
        if (start.get("phase") != "start" or finish.get("phase") != "finish"
                or any(PureWindowsPath(row["destination"]) != stage / rel for row in (start, finish))):
            raise ValueError(f"process journal is detached or out of order: {rel}")
        if spec is not None:
            _terminal_pair(directory, protocol, stage, spec, start, finish, allocations)
        elif start.get("allocation_scenario") is not None or start.get("allocation_samples") is not None:
            raise ValueError("unexpected allocation routing on paired command")
        raw = _json((directory/rel).with_suffix(".resources.json"))["direct_process"]
        lifetime = (raw["CreationTime100ns"], raw["ExitTime100ns"])
        if lifetime in lifetimes:
            raise ValueError("process receipt reused by multiple workloads")
        lifetimes.add(lifetime)
    if previous > host_times[-1]:
        raise ValueError("final host observation precedes terminal process completion")
