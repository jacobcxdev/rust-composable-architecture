"""Exercise the source selection and edition boundaries of the formatting gate."""

import pathlib
import subprocess
import sys
import tempfile
import unittest


CHECKER = pathlib.Path(__file__).resolve().parents[1] / "check_rust_format.py"


class RustFormatTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="rca-format-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = pathlib.Path(self.directory.name)
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True)
        self.write("Cargo.toml", '[workspace]\n[workspace.package]\nedition = "2024"\n')
        self.write("rust-toolchain.toml", '[toolchain]\nchannel = "1.98.0"\n')
        self.write("rustfmt.toml", 'newline_style = "Unix"\n')

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def check(self):
        return subprocess.run(
            [sys.executable, str(CHECKER)],
            cwd=self.root,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_unreferenced_untracked_source_is_checked_without_rewriting_it(self):
        self.write("src/lib.rs", "pub fn example() {}\n")
        subprocess.run(["git", "add", "src/lib.rs"], cwd=self.root, check=True)
        self.write("src/space name.rs", "fn probe( ){let value=1;}\n")
        self.write(".gitignore", "target/\n")
        self.write("target/generated.rs", "this deliberately cannot parse\n")
        original = (self.root / "src/space name.rs").read_bytes()
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("space name.rs", result.stdout + result.stderr)
        self.assertEqual((self.root / "src/space name.rs").read_bytes(), original)
        self.write("src/space name.rs", "fn probe() {\n    let value = 1;\n}\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Checked 2 Rust files", result.stdout)

    def test_nested_package_preserves_its_2021_parser_edition(self):
        self.write("nested/Cargo.toml", '[package]\nname = "nested"\nedition = "2021"\n')
        self.write("nested/src/lib.rs", "pub fn gen() {}\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_shared_comparison_fragment_uses_its_consumers_edition(self):
        self.write("tests/composable-counter/shared.rs", "pub fn gen() {}\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_workspace_inheritance_and_invalid_source_fail_closed(self):
        self.write("member/Cargo.toml", '[package]\nname = "member"\nedition.workspace = true\n')
        self.write("member/src/lib.rs", "pub fn gen() {}\n")
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("gen", result.stdout + result.stderr)

    def test_nested_workspace_supplies_its_own_inherited_edition(self):
        self.write("nested/Cargo.toml", '[workspace]\n[workspace.package]\nedition = "2021"\n')
        self.write("nested/member/Cargo.toml", '[package]\nname = "member"\nedition.workspace = true\n')
        self.write("nested/member/src/lib.rs", "pub fn gen() {}\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_missing_pinned_toolchain_is_a_failure(self):
        self.write("rust-toolchain.toml", '[toolchain]\nchannel = "rca-format-test-uninstalled"\n')
        self.write("src/lib.rs", "pub fn example() {}\n")
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rca-format-test-uninstalled", result.stdout + result.stderr)

    def test_symlink_source_is_not_followed(self):
        self.write("outside.txt", "pub fn example() {}\n")
        (self.root / "linked.rs").symlink_to(self.root / "outside.txt")
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symlink", result.stdout + result.stderr)

    def test_empty_source_selection_does_not_pass(self):
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No Rust source files", result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
