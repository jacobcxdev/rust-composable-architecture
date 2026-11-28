"""Keep publication credentials away from the pushed source and its build."""

import json
from pathlib import Path
import subprocess
import unittest


WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"


def workflow(filename):
    return json.loads(subprocess.check_output([
        "ruby", "-ryaml", "-rjson", "-e", "puts YAML.load_file(ARGV.fetch(0)).to_json",
        str(WORKFLOWS / filename),
    ]))


class DocumentationWorkflowTests(unittest.TestCase):
    def test_push_only_notifies_without_privileges(self):
        notifier = workflow("docs-push.yml")
        # Ruby's YAML parser reads the Actions key 'on' as the boolean true.
        self.assertEqual(notifier["true"], "push")
        self.assertEqual(notifier["permissions"], {})
        self.assertNotIn("secrets.", str(notifier))
        for job in notifier["jobs"].values():
            self.assertNotIn("environment", job)
            self.assertNotIn("permissions", job)

    def test_only_the_publication_job_can_read_site_credentials(self):
        publisher = workflow("publish-docs.yml")
        self.assertEqual(publisher["permissions"], {"contents": "read"})
        self.assertFalse({"push", "pull_request", "pull_request_target"} & publisher["true"].keys())
        self.assertEqual(publisher["true"]["workflow_run"]["workflows"], [workflow("docs-push.yml")["name"]])
        jobs = publisher["jobs"]
        for name, job in jobs.items():
            if name == "publish-docs":
                self.assertEqual(job["environment"], "documentation")
            else:
                self.assertNotIn("environment", job)
                self.assertNotIn("secrets.", str(job))

    def test_signer_accepts_only_its_build_and_resolved_ref(self):
        signer = workflow("publish-docs.yml")["jobs"]["publish-docs"]
        self.assertEqual(set(signer["needs"]), {"resolve", "build-docs"})
        for step in signer["steps"]:
            if step.get("uses", "").startswith("actions/checkout@"):
                options = step["with"]
                self.assertFalse(options["persist-credentials"])
                self.assertNotIn("${{", options.get("ref", ""))  # Never select the pushed source.
        download = next(step["with"] for step in signer["steps"]
                        if step.get("uses", "").startswith("actions/download-artifact@"))
        self.assertEqual(download["artifact-ids"], "${{ needs.build-docs.outputs.artifact-id }}")
        self.assertNotIn("run-id", download)
        self.assertEqual(download["digest-mismatch"], "error")
        publish = next(step["with"] for step in signer["steps"]
                       if step.get("uses", "").endswith("/publish-docs"))
        self.assertEqual(publish["source-commit"], "${{ needs.resolve.outputs.commit }}")
        self.assertEqual(publish["source-ref-name"], "${{ needs.resolve.outputs.name }}")
        self.assertEqual(publish["source-ref-type"], "${{ matrix.kind }}")
        self.assertEqual(publish["operation"], "${{ needs.resolve.outputs.operation }}")
        self.assertEqual(publish["build-command"], "true")

    def test_credential_validation_cannot_publish(self):
        publisher = workflow("publish-docs.yml")
        validation = publisher["true"]["workflow_dispatch"]["inputs"]["validate_only"]
        self.assertEqual(validation["type"], "boolean")
        self.assertFalse(validation["default"])
        steps = publisher["jobs"]["publish-docs"]["steps"]
        probe = next(step for step in steps if "push --dry-run" in step.get("run", ""))
        publish = next(step["with"] for step in steps
                       if step.get("uses", "").endswith("/publish-docs"))
        token = "${{ secrets.INDEX_REPO_TOKEN }}"
        validation_only = "github.event_name == 'workflow_dispatch' && inputs.validate_only"
        self.assertEqual(probe["env"]["GH_TOKEN"], token)
        self.assertEqual(probe["if"], validation_only)
        self.assertEqual(publish["token"], token)
        self.assertEqual(publish["dry-run"], "${{ " + validation_only + " }}")
