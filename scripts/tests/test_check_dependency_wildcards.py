from pathlib import Path
import tempfile
import unittest

from scripts.check_dependency_wildcards import check


class DependencyWildcardTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.manifest = self.root / "rca-macros" / "Cargo.toml"
        self.manifest.parent.mkdir()
        self.manifest.write_text('[dev-dependencies]\nrca = { package = "rca-core", path = "../rca-core" }\n'
                                 'rca-test = { path = "../rca-test" }\n')

    def graph(self, *, alias="rca", name="rca-core", owner="rca-macros", kind="dev",
              target=None, private=False, source=None, resolved=None, path=None, req="*"):
        dep = {"name": name, "rename": alias if alias != name else None, "req": req,
               "source": source, "kind": kind, "target": target, "path": str(self.root / name)}
        packages = [
            {"name": owner, "id": "owner", "source": None, "manifest_path": str(self.root / owner / "Cargo.toml"),
             "publish": [] if private else None, "dependencies": [dep]},
            {"name": name, "id": "dependency", "source": resolved, "manifest_path": str((path or self.root / name) / "Cargo.toml")},
        ]
        edge = {"name": alias.replace("-", "_"), "pkg": "dependency", "dep_kinds": [{"kind": kind, "target": target}]}
        return {"workspace_root": str(self.root), "packages": packages,
                "resolve": {"nodes": [{"id": "owner", "deps": [edge]}]}}

    def test_accepts_both_workspace_only_test_dependencies(self):
        check(self.graph())
        check(self.graph(alias="rca-test", name="rca-test"))

    def test_rejects_other_newly_exempt_edges(self):
        for change in ({"owner": "other"}, {"alias": "other"}, {"name": "other"},
                       {"kind": None, "private": True}, {"kind": "build", "private": True},
                       {"target": "cfg(unix)"}, {"path": self.root.parent / "rca-core"},
                       {"source": "git+https://example.invalid/crate", "resolved": "git+https://example.invalid/crate"},
                       {"source": "registry+https://index.crates.io"}):  # Patched to a local package.
            with self.subTest(change=change), self.assertRaisesRegex(ValueError, "wildcard"):
                check(self.graph(**change))

    def test_rejects_explicit_star_in_otherwise_matching_declaration(self):
        self.manifest.write_text('[dev-dependencies]\nrca = { package = "rca-core", path = "../rca-core", version = "*" }\n')
        with self.assertRaisesRegex(ValueError, "wildcard"):
            check(self.graph())

    def test_leaves_unchanged_cargo_deny_cases_alone(self):
        for change in ({"resolved": "registry+https://index.crates.io"}, {"kind": None},
                       {"kind": "build"}, {"req": "=0.1.0"}):
            with self.subTest(change=change):
                check(self.graph(owner="other", **change))
        graph = self.graph(owner="other")
        graph["resolve"]["nodes"][0]["deps"].clear()  # Unused third-party declaration.
        check(graph)
