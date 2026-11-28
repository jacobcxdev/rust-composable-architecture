"""Exercise the release upload loop without a registry, credentials or real Cargo."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "publish_missing_crates.sh"

# Each double records calls in the isolated checkout. A partial upload changes
# what the next registry query sees. There is no network or real sleep.
DRIVER = '''
import hashlib, json, os, subprocess, sys
from pathlib import Path
root = Path.cwd()
statefile = root / "state.json"
state = json.loads(statefile.read_text())
name = Path(sys.argv[0]).name
args = sys.argv[1:]
with (root / "calls").open("a") as out:
    print(json.dumps([name, *args]), file=out)
if name == "check_release_tag.py":
    if state.get("stale_tag"):
        sys.exit(1)
    print(state["version"])
elif name == "check_registry_archives.py":
    if state.get("bad_index"):
        sys.exit("registry checksum mismatch")
    if "--require-complete" in args and (state["missing"] or state.get("lag")):
        state["lag"] = False
        statefile.write_text(json.dumps(state))
        sys.exit("missing from crates.io: rca-core")
    print(*state["missing"], sep="\\n", end="\\n" if state["missing"] else "")
elif name == "cargo":
    assert "--locked" in args and "--no-verify" in args and args[args.index("--registry")+1] == "crates-io"
    names = [args[i+1] for i, arg in enumerate(args) if arg == "--package"]
    assert names == state["missing"]
    dest = Path(args[args.index("--target-dir")+1]) / "package"
    assert dest.resolve() != (root / "target/package").resolve()
    if args[0] == "publish":
        assert "CARGO_REGISTRY_TOKEN" not in os.environ
        assert args[args.index("--config")+1] == 'registry.credential-provider="cargo_release_credential.py"'
        provider = root / "scripts/cargo_release_credential.py"
        for crate in names:
            checksum = hashlib.sha256(b"wrong" if state.get("drift") else crate.encode()).hexdigest()
            request = {"kind":"get", "v":1, "operation":"publish", "name":crate,
                       "vers":state["version"], "cksum":checksum,
                       "registry":{"index-url":"https://github.com/rust-lang/crates.io-index"}}
            response = subprocess.run([provider, "--cargo-plugin"], input=json.dumps(request)+"\\n",
                                      capture_output=True, text=True, check=True)
            result = json.loads(response.stdout.splitlines()[1])
            if "Err" in result:
                sys.exit("credential refused")
            assert result["Ok"]["token"] == "fixture-not-a-secret"
        outcome = state["outcomes"].pop(0)
        if outcome == "timeout":
            sys.exit("connection reset by peer")
        if outcome.startswith("429"):
            if outcome == "429":
                state["missing"] = names[1:]
            statefile.write_text(json.dumps(state))
            sys.exit(f"error: failed to publish {names[-1]} v{state['version']} to registry at https://crates.io\\nCaused by:\\n"
                     "  the remote server responded with an error (status 429 Too Many Requests)")
        state["missing"] = []
        statefile.write_text(json.dumps(state))
    else:
        sys.exit("expected only one packaging pass through cargo publish")
'''


class PublishTests(unittest.TestCase):
    def run_upload(self, missing=("rca-core", "rca-test"), outcomes=("ok",), version="0.1.0", token=True, **modes):
        with TemporaryDirectory() as temp:
            root = Path(temp)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(SCRIPT, scripts / SCRIPT.name)
            shutil.copy2(SCRIPT.with_name("cargo_release_credential.py"), scripts)
            stage = root / "target/package"
            stage.mkdir(parents=True)
            for crate in missing:
                (stage / f"{crate}-{version}.crate").write_bytes(crate.encode())
            for name in ("cargo", "sleep", "check_registry_archives.py", "check_release_tag.py"):
                fake = scripts / name
                fake.write_text(f"#!{sys.executable}\n" + DRIVER)
                fake.chmod(0o700)
            (root / "state.json").write_text(json.dumps(dict(missing=missing, outcomes=outcomes, version=version, **modes)))
            env = {"PATH": f"{scripts}:{Path(sys.executable).parent}:/usr/bin:/bin", "GITHUB_SHA": "a"*40,
                   "GITHUB_REF_NAME": "v"+version, "TMPDIR": temp, "RUNNER_TEMP": temp}
            if token:
                env["CARGO_REGISTRY_TOKEN"] = "fixture-not-a-secret"
            result = subprocess.run(["bash", str(scripts / SCRIPT.name), version], cwd=root, env=env,
                                    capture_output=True, text=True, check=False)
            calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
            self.assertNotIn("fixture-not-a-secret", result.stdout + result.stderr + str(calls))
            return result, calls

    def test_matching_partial_release_uploads_only_the_remainder(self):
        result, calls = self.run_upload(missing=["rca-test"])
        self.assertEqual(result.returncode, 0, result.stderr)
        kinds = [c[1] if c[0] == "cargo" else c[0] for c in calls]
        upload = kinds.index("publish")
        self.assertEqual(kinds[upload-1], "check_release_tag.py")
        self.assertIn("--require-complete", calls[-1])
        self.assertNotIn("sleep", kinds)

    def test_missing_token_stops_before_packaging_or_upload(self):
        result, calls = self.run_upload(token=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CARGO_REGISTRY_TOKEN", result.stderr)
        self.assertFalse(any(c[0] == "cargo" for c in calls))

    def test_first_release_rechecks_after_rate_limit_and_does_not_reupload(self):
        result, calls = self.run_upload(outcomes=["429", "ok"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(["sleep", "600"], calls)
        attempts = [call for call in calls if call[:2] == ["cargo", "publish"]]
        self.assertEqual([c.count("--package") for c in attempts], [2, 1])
        self.assertNotIn("rca-core", attempts[1])
        self.assertEqual(sum(call[0] == "check_release_tag.py" for call in calls), 2)

    def test_refuses_changed_archives_stale_tags_or_registry_conflicts(self):
        for reason in ("bad_index", "stale_tag"):
            result, calls = self.run_upload(**{reason: True})
            with self.subTest(reason=reason):
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(call[:2] == ["cargo", "publish"] for call in calls))

    def test_actual_upload_checksum_must_match_approved_bytes(self):
        result, calls = self.run_upload(drift=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("credential refused", result.stderr)
        self.assertFalse(any(call[0] == "sleep" for call in calls))

    def test_no_retry_on_ambiguous_upload_or_rate_limit_on_later_version(self):
        for outcome, version in [("timeout", "0.1.0"), ("429", "0.1.1")]:
            result, calls = self.run_upload(outcomes=[outcome], version=version)
            with self.subTest(version=version):
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(any(call[0] == "sleep" for call in calls))

    def test_repeated_first_release_rate_limits_stop_at_eight_waits(self):
        result, calls = self.run_upload(outcomes=["429-stalled"] * 9)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls.count(["sleep", "600"]), 8)
        self.assertEqual(sum(call[:2] == ["cargo", "publish"] for call in calls), 9)

    def test_completed_release_never_invokes_cargo_and_slow_index_only_waits_once(self):
        result, calls = self.run_upload(missing=[], token=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(c[0] in ("cargo", "sleep") for c in calls))
        result, calls = self.run_upload(lag=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sum(c[0] == "sleep" for c in calls), 1)
