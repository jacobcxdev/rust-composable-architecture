"""Exercise the canonical runner check without building or collecting samples."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
QUALIFIER = REPO_ROOT / "scripts" / "qualify-performance.sh"

# Each replacement supplies one external preflight observation. The first
# toolchain lookup stops execution, before any build or native capture can run.
PREFLIGHT = r"""
git() {
    case "$*" in
        *--show-toplevel*) printf '%s\n' "$RCA_TEST_ROOT" ;;
        *status*) return 0 ;;
        *) printf 'unexpected git probe: %s\n' "$*" >&2; return 98 ;;
    esac
}
uname() {
    case "$1" in -s) printf 'Darwin\n' ;; -m) printf 'arm64\n' ;; esac
}
pgrep() { return 1; }
sysctl() {
    case "$2" in
        hw.model) printf 'Mac15,9\n' ;;
        machdep.cpu.brand_string) printf 'Apple M3 Max\n' ;;
        hw.logicalcpu) printf '16\n' ;;
        hw.memsize) printf '51539607552\n' ;;
        *) return 98 ;;
    esac
}
sw_vers() {
    case "$1" in
        -productVersion) printf '%s\n' "$RCA_TEST_OS_VERSION" ;;
        -buildVersion) printf '%s\n' "$RCA_TEST_OS_BUILD" ;;
        *) return 98 ;;
    esac
}
pmset() {
    case "$2" in
        batt) printf "Now drawing from 'AC Power'\n" ;;
        custom) printf 'AC Power:\n powermode 2\n' ;;
        *) return 98 ;;
    esac
}
rustup() {
    printf 'cohort accepted; stopped before toolchain lookup\n' >&2
    return 91
}
xcrun() { return 98; }
export -f git uname pgrep sysctl sw_vers pmset rustup xcrun
exec bash "$RCA_TEST_QUALIFIER" "$RCA_TEST_ROOT/candidate.json" \
    "$RCA_TEST_ROOT" "$RCA_TEST_ROOT/reference.json"
"""


class CanonicalPerformanceCohortTests(unittest.TestCase):
    def preflight(self, version, build, runner=None):
        with tempfile.TemporaryDirectory(prefix="rca-cohort-test-") as directory:
            environment = dict(os.environ)
            for name in ("RCA_PERF_RUNNER", "RCA_QUALIFY_ALLOCATIONS", "RCA_DHAT_FILE"):
                environment.pop(name, None)
            environment.update(
                RCA_TEST_ROOT=directory,
                RCA_TEST_QUALIFIER=str(QUALIFIER),
                RCA_TEST_OS_VERSION=version,
                RCA_TEST_OS_BUILD=build,
            )
            if runner is not None:
                environment["RCA_PERF_RUNNER"] = runner
            return subprocess.run(
                ["bash", "-c", PREFLIGHT],
                cwd=REPO_ROOT,
                env=environment,
                text=True,
                capture_output=True,
                check=False,
            )

    def test_installed_macos27_cohort_reaches_the_pinned_toolchain_check(self):
        result = self.preflight("27.0", "26A428")
        self.assertEqual(result.returncode, 91, result.stderr)
        self.assertIn("cohort accepted; stopped before toolchain lookup", result.stderr)

    def test_other_os_cohorts_stop_before_any_build(self):
        for version, build in (("26.0", "25A354"), ("28.0", "27A100")):
            with self.subTest(version=version):
                result = self.preflight(version, build)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("host does not match pinned runner", result.stderr)
                self.assertNotIn("cohort accepted", result.stderr)

    def test_old_runner_identity_cannot_label_the_current_host(self):
        result = self.preflight(
            "27.0", "26A428", "mac15-9-m3-max-16c-48gb-macos26-ac-high"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(
            "runner must be pinned to mac15-9-m3-max-16c-48gb-macos27-ac-high",
            result.stderr,
        )
        self.assertNotIn("cohort accepted", result.stderr)


if __name__ == "__main__":
    unittest.main()
