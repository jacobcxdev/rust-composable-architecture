"""Join identities after independent native and composition replay; never issue a verdict."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

from composition_performance import RUST100_TOOL_VERSIONS
import native_release_reference

ROOT = Path(__file__).resolve().parents[1]
RESULT = "experimental-rank-result.json"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def git(*args):
    return subprocess.check_output(["git", "--no-replace-objects", *args], cwd=ROOT,
                                   stderr=subprocess.PIPE)


def git_blob_hash(revision, path):
    return hashlib.sha256(git("show", f"{revision}:{path}")).hexdigest()


def signed_source(revision):
    require(re.fullmatch("[0-9a-f]{40}", revision), "a full signed Git commit is required")
    try:
        git("verify-commit", revision)
    except subprocess.CalledProcessError as exc:
        raise ValueError(f"not a verified signed commit: {revision}") from exc
    return git("rev-parse", revision + "^{tree}").decode().strip(), git_blob_hash(revision, "Cargo.lock")


def read(path, expected=...):
    require(path.is_file() and path.resolve() == path.absolute(), f"missing or redirected input: {path}")
    data = path.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if expected is not ...:
        require(digest == expected, f"changed input: {path}")
    return json.loads(data), digest


def check_inputs(native, composition, support, controller, subject, reference):
    """Check original Git inputs and machine/tool identities, not support completeness or WPR."""
    selected = native_release_reference.composition_reference(subject)
    require(reference == selected["revision"], "capture differs from the selected reference")
    expected_mode = "first-release-enrolment" if subject == reference else "release-comparison"
    require(selected["mode"] == expected_mode, "reference mode does not match the source identities")
    native_result = read(native / RESULT)[0]
    meta = read(native / "inputs.json", native_result["inputs_sha256"])[0]["metadata"]
    build, build_hash = read(native / "source-build.json", native_result["source_build_sha256"])
    composition_result = read(composition / RESULT)[0]
    capture = read(composition / "capture.json", composition_result["capture_sha256"])[0]
    require(capture.get("reference_kind") == composition_result.get("reference_kind") == expected_mode
            and composition_result.get("prior_release_comparison") is (subject != reference)
            and composition_result.get("reference_revision") == reference,
            "composition reference identity or comparison claim differs from the selected reference")
    arm = "candidate-reference"
    tools = read(composition / arm / "inputs.json", capture["modes"][arm]["inputs.json"])[0]["tools"]
    support = read(support)[0]
    require(capture["experiment"]["method"] == "finite-rank-v3", "expected finite-rank-v3 composition")
    for name, role, revision in (("controller", None, controller), ("measured_subject", "candidate", subject),
                                 ("control", "reference", reference)):
        tree, lock = signed_source(revision)
        if role == "reference":
            require((tree, lock) == (selected["tree"], selected["lock_sha256"]),
                    "selected reference tree or lock differs from signed Git input")
        identity = capture[name]
        require((identity["revision"], identity["tree"], identity["hashes"]["Cargo.lock"]) ==
                (revision, tree, lock), f"composition {name} differs from signed Git input")
        if role:
            source = build[role]
            require((source["revision"], source["tree"], source["lock_sha256"]) == (revision, tree, lock),
                    f"native {role} differs from signed Git input")
    require(meta["cohort"]["qualification_script_sha256"] == git_blob_hash(controller, "scripts/capture-windows-performance.py"),
            "native capture script differs from selected controller")
    require(support["method"] == "windows-runtime-support-v1" and support["candidate"] == build["candidate"]
            and support["source_build"]["sha256"] == build_hash, "different support candidate or native source-build")
    host = capture["host"]["identity"]
    support_host = support["host"]["identity"]
    for key in ("system", "hostname", "os", "machine_model", "cpu", "logical_cpus"):
        require(host[key] == meta[key] == meta["cohort"][key] == support_host[key], f"different host {key}")
    for key in ("os_version", "os_build", "platform", "target"):
        require(host[key] == support_host[key], f"different support host {key}")
    require(host["os_version"] in host["os"].split("-") and host["os_version"].split(".")[-1] == host["os_build"],
            "native host OS does not identify the composition build")
    tc = build["toolchain"]
    require(tc == support["toolchain"] and tc["target"] == host["target"] == "x86_64-pc-windows-msvc"
            and all(tc[k] == RUST100_TOOL_VERSIONS[k] for k in ("rustc", "cargo")), "different toolchain or target")
    for name in ("rustc", "cargo", "rustdoc"):
        digest = tools[name]["sha256"]
        require(re.fullmatch("[0-9a-f]{64}", digest) and digest == support["tools"][name]["sha256"],
                f"different tool executable: {name}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("native", "composition", "support", "controller", "subject", "reference"):
        parser.add_argument("--" + name, required=True, type=Path if name in ("native", "composition", "support") else str)
    try:
        args = parser.parse_args()
        check_inputs(**vars(args))
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"Windows performance input mismatch: {exc}\n")
    if args.subject == args.reference:
        print("First-release input identities match. Both arms use the same signed candidate. "
              "No comparison with a previous release and no performance verdict.")
    else:
        print("Windows performance input identities match; no performance verdict.")
