"""Check documentation routing against live Git refs and delayed GitHub events."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from scripts import resolve_doc_refs as docs


REPO = {"id": 42, "full_name": "sample/project", "default_branch": "main"}
COMMIT, PRIOR = "a" * 40, "b" * 40


def push_event(**run):
    return {"repository": REPO, "action": "completed", "workflow_run": {
        "head_repository": REPO, "status": "completed", "event": "push", "conclusion": "success",
        "path": docs.PUSH_WORKFLOW, "head_branch": "release/next", "head_sha": COMMIT, **run,
    }}


def repair(kind, name="release/next"):
    return {"repository": REPO, "ref": "refs/heads/main", "inputs": {"name": name, "kind": kind}}


def route(kind, event, live, **kwargs):
    return docs.resolve(kind, event, REPO["full_name"], REPO["id"], lambda *_: live, **kwargs)


class RefResolution(unittest.TestCase):
    def test_push_matches_only_the_current_commit_in_each_namespace(self):
        for refs, kinds in [
            ({}, []), ({"branch": PRIOR}, []), ({"tag": COMMIT}, ["tag"]),
            ({"branch": COMMIT, "tag": PRIOR}, ["branch"]),
            ({"branch": PRIOR, "tag": COMMIT}, ["tag"]),
            ({"branch": COMMIT, "tag": COMMIT}, ["branch", "tag"]),
        ]:
            with self.subTest(refs=refs):
                expected = ("upsert", "release/next", COMMIT, kinds) if kinds else None
                self.assertEqual(route("workflow_run", push_event(), refs), expected)

    def test_other_workflows_and_forks_cannot_select_refs(self):
        for event in [
            push_event(head_repository=None), push_event(head_repository={"id": 5}),
            push_event(path=".github/workflows/ci.yml"), push_event(event="pull_request"),
            push_event(conclusion="failure"), push_event(status="in_progress"),
            dict(push_event(), action="in_progress"),
            dict(push_event(), repository=dict(REPO, id=41)),
            dict(push_event(), repository=dict(REPO, full_name="other/project")),
        ]:
            with self.subTest(event=event):
                self.assertIsNone(docs.resolve("workflow_run", event, REPO["full_name"], REPO["id"],
                                               lambda *_: self.fail("Unexpected ref lookup")))
        with self.assertRaises(ValueError):
            route("workflow_run", push_event(head_sha="main"), {"branch": COMMIT})

    def test_delete_checks_only_the_event_namespace_and_skips_recreated_refs(self):
        payload = {"repository": REPO, "ref": "release/next", "ref_type": "branch"}
        self.assertEqual(route("delete", payload, {"tag": COMMIT}), ("delete", "release/next", "", ["branch"]))
        self.assertIsNone(route("delete", payload, {"branch": PRIOR}))
        with self.assertRaises(ValueError):
            route("delete", dict(payload, ref_type="pull"), {})

    def test_manual_repair_uses_the_trusted_workflow_and_checks_tag_objects(self):
        self.assertIsNone(route("workflow_dispatch", dict(repair("tag"), ref="refs/tags/main"), {}))
        self.assertEqual(route("workflow_dispatch", repair("tag"), {"branch": COMMIT}),
                         ("delete", "release/next", "", ["tag"]))
        checked = []
        self.assertEqual(route("workflow_dispatch", repair("tag"), {"tag": COMMIT},
                               validate_commit=lambda *args: checked.append(args)),
                         ("upsert", "release/next", COMMIT, ["tag"]))
        self.assertEqual(checked, [(REPO["full_name"], COMMIT)])

    def test_git_reads_exact_refs_and_peels_tags_without_executing_the_name(self):
        with tempfile.TemporaryDirectory() as scratch:
            def git(*args, data=None):
                return subprocess.check_output(["git", "-C", scratch, *args],
                                               input=data, encoding="utf-8").strip()

            # Borrow the checkout's existing commit, including in a shallow CI checkout.
            # All fixture refs and the annotated tag object live only in the bare copy.
            subprocess.run(["git", "clone", "-q", "--bare", "--shared", "--no-tags", "--single-branch",
                            str(Path(__file__).resolve().parents[2]), scratch], check=True)
            commit = git("rev-parse", "HEAD")
            name = "group/rock'n;roll+$value"
            tag = git("mktag", data=f"object {commit}\ntype commit\ntag {name}\n"
                                    "tagger Example <example@example.org> 1760000000 +0000\n\nTest\n")
            git("update-ref", "refs/tags/" + name, tag)
            git("update-ref", "refs/tags/light", commit)
            git("update-ref", "refs/heads/" + name, commit)
            git("update-ref", "refs/heads/other/refs/heads/short", commit)
            self.assertEqual(docs.read_refs(scratch, name), {"branch": commit, "tag": commit})
            self.assertEqual(docs.read_refs(scratch, "light"), {"tag": commit})
            self.assertEqual(docs.read_refs(scratch, "short"), {})
            for invalid in ["../main", "a\nactive=true", "a\rb", "release/*", "v^{commit}", ""]:
                with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                    docs.read_refs(scratch, invalid)
            with self.assertRaises(subprocess.CalledProcessError):
                docs.read_refs(str(Path(scratch) / "missing-repository"), "valid")

    def test_inactive_actions_outputs_cannot_retain_an_old_operation(self):
        with tempfile.TemporaryDirectory() as scratch:
            event, output = (Path(scratch) / name for name in ("event.json", "outputs"))
            event.write_text(json.dumps(push_event(event="pull_request")))
            env = {"GITHUB_EVENT_NAME": "workflow_run", "GITHUB_EVENT_PATH": str(event),
                   "GITHUB_OUTPUT": str(output), "GITHUB_REPOSITORY": REPO["full_name"],
                   "GITHUB_REPOSITORY_ID": str(REPO["id"])}
            with patch.dict(os.environ, env):
                docs.main()
            self.assertEqual(output.read_text(), "active=false\noperation=\nname=\ncommit=\nkinds=[]\n")
