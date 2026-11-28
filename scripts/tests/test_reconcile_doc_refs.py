"""Exercise the daily documentation repair path without contacting GitHub."""

from contextlib import redirect_stdout
from io import BytesIO, StringIO
import json
import os
import subprocess
import sys
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

from scripts import reconcile_doc_refs as repair


REPO, ID, DEFAULT = "example/project", 42, "main"
A, B, C = [character * 40 for character in "abc"]


def inventory(*rows, repository_id=ID):
    return {"repositoryId": repository_id, "publications": [
        {"refType": kind, "refName": name, "commitSha": commit} for kind, name, commit in rows
    ]}


def response(data):
    return BytesIO(json.dumps(data).encode())


class ReconcileDocRefs(unittest.TestCase):
    def test_one_advertisement_separates_namespaces_and_peels_annotated_tags(self):
        rows = "\n".join([
            f"{A}\trefs/heads/main", f"{A}\trefs/tags/main", f"{B}\trefs/tags/feature/a^{{}}",
            f"{C}\trefs/tags/feature/a", f"{A}\trefs/heads/feature/a",
            f"{C}\trefs/heads/release/a\u2028b",
        ])
        self.assertEqual(repair.parse_refs(rows, DEFAULT), {
            ("branch", "main"): A, ("tag", "main"): A,
            ("branch", "feature/a"): A, ("tag", "feature/a"): B,
            ("branch", "release/a\u2028b"): C,
        })
        for bad in ("", f"{A}\trefs/heads/dev", rows + f"\n{C}\trefs/heads/main",
                    rows + f"\n{C}\trefs/tags/orphan^{{}}", rows + "\nbad\trefs/heads/x"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                repair.parse_refs(bad, DEFAULT)

    def test_incomplete_unavailable_or_wrong_site_inventory_never_dispatches(self):
        row = ("branch", "a", A)
        with patch.object(repair, "live_refs", return_value={("branch", DEFAULT): A}), \
                patch.object(subprocess, "run", wraps=subprocess.run) as run:
            for invalid in ({"publications": []}, inventory(repository_id=13),
                            {"repositoryId": ID, "publications": {}}, inventory(row, row),
                            inventory(("branch", "../x", A)), inventory(("branch", "x", "bad"))):
                with self.subTest(invalid=invalid), patch.object(repair, "urlopen", return_value=response(invalid)), \
                        self.assertRaises((KeyError, ValueError)):
                    repair.reconcile(REPO, ID, DEFAULT, True)
            for status in (404, 429, 503):
                with self.subTest(status=status), patch.object(
                    repair, "urlopen", side_effect=HTTPError("https://site", status, "", {}, None)
                ), self.assertRaises(HTTPError):
                    repair.reconcile(REPO, ID, DEFAULT, True)
            self.assertFalse(any(call.args[0][0] == "gh" for call in run.call_args_list))

    def test_source_failure_never_requests_deletion_and_empty_site_must_be_explicit(self):
        with patch.object(repair, "live_refs", side_effect=subprocess.CalledProcessError(128, ["git"])), \
                patch.object(repair, "published_refs") as site, patch.object(subprocess, "run") as dispatch, \
                self.assertRaises(subprocess.CalledProcessError):
            repair.reconcile(REPO, ID, DEFAULT, True)
        site.assert_not_called()
        dispatch.assert_not_called()
        with patch.object(repair, "urlopen", return_value=response(inventory())):
            self.assertEqual(repair.published_refs(ID), {})

    def test_only_differences_dispatch_and_dry_run_never_writes(self):
        name = "release/a'b;$value"
        unchanged = {("branch", DEFAULT): A}
        live = unchanged | {("branch", name): A, ("tag", name): B}
        site = unchanged | {("tag", name): A, ("tag", "retired"): C}
        with patch.object(repair, "live_refs", return_value=live), \
                patch.object(repair, "published_refs", return_value=site), \
                patch.object(subprocess, "run") as calls, redirect_stdout(StringIO()):
            repair.reconcile(REPO, ID, DEFAULT, False)
            calls.assert_not_called()
            repair.reconcile(REPO, ID, DEFAULT, True)
            expected = [("branch", name), ("tag", name), ("tag", "retired")]
            self.assertEqual([call.args[0] for call in calls.call_args_list], [
                ["gh", "workflow", "run", "publish-docs.yml", "--repo", REPO, "--ref", DEFAULT,
                 "-f", f"kind={kind}", "-f", f"name={ref}"] for kind, ref in expected
            ])
            with patch.object(repair, "published_refs", return_value=live):
                calls.reset_mock()
                repair.reconcile(REPO, ID, DEFAULT, True)
                calls.assert_not_called()

    def test_dispatch_rejects_non_default_branch_and_non_workflow_invocation(self):
        env = {"DEFAULT_BRANCH": DEFAULT, "GITHUB_REF": "refs/heads/main",
               "GITHUB_EVENT_NAME": "schedule", "GITHUB_ACTIONS": "true"}
        for changed in ({"GITHUB_REF": "refs/heads/other"}, {"GITHUB_EVENT_NAME": "push"}, {"GITHUB_ACTIONS": ""}):
            with self.subTest(changed=changed), patch.dict(os.environ, env | changed, clear=True), \
                    patch.object(sys, "argv", ["repair", "--dispatch"]), patch.object(sys, "stderr", StringIO()), \
                    patch.object(repair, "reconcile") as run, self.assertRaises(SystemExit):
                repair.main()
            run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
