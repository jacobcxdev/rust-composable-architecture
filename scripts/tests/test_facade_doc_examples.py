"""Check that facade qualification uses the examples' real consumers and setup."""

import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class FacadeDocExamples(unittest.TestCase):
    def test_checkout_consumers_resolve_local_paths_to_the_documented_packages(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "consumers"
            subprocess.run([sys.executable, ROOT / "scripts/facade_doc_examples.py", out],
                           check=True, stdout=subprocess.DEVNULL)
            found = set()
            for manifest in out.glob("consumer-*/Cargo.toml"):
                cfg = tomllib.loads(manifest.read_text())
                for table in ("dependencies", "dev-dependencies"):
                    for alias, dep in cfg.get(table, {}).items():
                        if isinstance(dep, dict) and "path" in dep:
                            package = tomllib.loads((Path(dep["path"]) / "Cargo.toml").read_text())
                            self.assertEqual(package["package"]["name"], dep.get("package", alias))
                            found.add(package["package"]["name"])
            self.assertEqual(found, {"rca", "rca-test"})

    def test_generates_independent_consumers_and_keeps_rustdoc_contracts(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "consumers"
            subprocess.run([sys.executable, ROOT / "scripts/facade_doc_examples.py", out,
                            "--registry", "rca-local"], check=True)
            cases = {}
            rust_version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["rust-version"]
            for manifest in out.glob("consumer-*/Cargo.toml"):
                cfg = tomllib.loads(manifest.read_text())
                self.assertEqual(cfg["package"]["rust-version"], rust_version)
                cases[manifest.parent.name] = (cfg["dependencies"], (manifest.parent / "examples.md").read_text())
            minimal, docs = cases["consumer-minimal"]
            self.assertEqual(set(minimal), {"rca"})
            self.assertEqual(minimal["rca"]["registry"], "rca-local")
            self.assertIn("use rca::", docs)
            self.assertIn("```compile_fail,E0308", docs)
            self.assertTrue("Imports for the rejection examples" in docs, "missing positive import controls")
            imports = docs.split("Imports for the rejection examples", 1)[1]
            self.assertIn("use rca::DependencyDefaults;", imports)
            self.assertNotIn("compile_fail", imports)
            hosted = cases["consumer-test-util-executor"][1]
            for name in ("count_once", "route_actions", "take_product"):
                self.assertIn(f"async fn {name}(runtime: Runtime)", docs)
                self.assertIn(f"host.run({name}(host.runtime()))", hosted)
            self.assertNotIn("host.run(", docs)
            self.assertIn("consumer-local", cases)
            self.assertIn("consumer-diagnostics", cases)
            self.assertIn("consumer-test-util", cases)
            self.assertEqual(set(cases["consumer-tokio"][0]), {"rca", "tokio"})
            alias, macro_docs = cases["consumer-alias-macros"]
            self.assertEqual(set(alias), {"architecture"})
            self.assertEqual(alias["architecture"]["features"], ["macros"])
            self.assertIn("use architecture::", macro_docs)
            self.assertNotIn(" as rca;", macro_docs)
            self.assertIn("futures_executor::block_on", "".join(body for _, body in cases.values()))
            for _, body in cases.values():
                self.assertNotIn("/tests/support/mod.rs", body)
                self.assertNotIn("# mod rca {", body)
                self.assertNotIn("# use rca_core as rca;", body)

    def test_preserves_real_hidden_setup_and_rejects_unknown_shims(self):
        from scripts.facade_doc_examples import consumer_body

        source = ("# mod rca { pub use rca_core::Runtime; pub use rca_test::TestStore; }\n"
                  "# let setup = 1;\nuse rca::TestStore;\nassert_eq!(setup, 1);")
        self.assertEqual(consumer_body(source), "# let setup = 1;\nuse rca::TestStore;\nassert_eq!(setup, 1);")
        with self.assertRaisesRegex(ValueError, "unrecognised.*shim"):
            consumer_body("# mod rca { pub fn fake() {} }\nrca::fake();")

    def test_rejects_public_setup_after_a_private_main(self):
        from scripts.facade_doc_examples import runtime_example

        source = ('async fn route_actions(runtime: Runtime) {}\n# fn main() {\n'
                  '# include!("/tests/support/mod.rs");\nlet visible = 1;\n# }')
        with self.assertRaisesRegex(ValueError, "completely hidden"):
            runtime_example(source)
