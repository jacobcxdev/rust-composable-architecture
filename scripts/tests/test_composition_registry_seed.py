"""Protect cold compile measurements and keep normal benchmark builds independent."""

import hashlib
import io
import json
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import composition_registry_seed as cache


class RegistrySeedTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name).resolve()
        self.donor = self.root / "candidate-reference/control"
        self.target = self.root / "reference-reference-aa/subject"
        self.log = self.root / "evidence/candidate-reference/builds/control.txt"
        self.log.parent.mkdir(parents=True)
        self.home = self.root / "cargo"
        registry = "registry+https://github.com/rust-lang/crates.io-index"
        self.identity = registry + "#ordinary@1.2.3"
        self.src = self.home / "registry/src/index/ordinary-1.2.3/src/lib.rs"
        self.src.parent.mkdir(parents=True)
        self.src.write_bytes(b"pub fn dependency() {}")
        self.archive = self.home / "registry/cache/index/ordinary-1.2.3.crate"
        self.archive.parent.mkdir(parents=True)
        with tarfile.open(self.archive, "w:gz") as stream:
            info = tarfile.TarInfo("ordinary-1.2.3/src/lib.rs")
            contents = self.src.read_bytes()
            info.size = len(contents)
            stream.addfile(info, io.BytesIO(contents))
        checksum = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        lock = (f'version = 4\n[[package]]\nname = "ordinary"\nversion = "1.2.3"\n'
                f'source = "{registry}"\nchecksum = "{checksum}"\n').encode()
        self.lock_sha = hashlib.sha256(lock).hexdigest()
        for t in (self.donor, self.target):
            self.lock(t).parent.mkdir(parents=True)
            self.lock(t).write_bytes(lock)
        self.unit = Path("release/build/ordinary/1234567890abcdef")
        self.compiled = self.unit / "out/libordinary.rlib"
        self.owned = Path("release/build/rca-core/fedcba0987654321/out/composition")
        for file in (self.compiled, self.unit / "fingerprint/lib-ordinary", self.owned):
            (self.donor / file).parent.mkdir(parents=True, exist_ok=True)
            (self.donor / file).write_bytes(b"original")
        self.path_id = "path+file:///owned/rca-core#0.1.0"
        self.rows = [self.artifact(self.identity, "ordinary", self.src, self.donor/self.compiled),
                     self.artifact(self.path_id, "composition",
                                   self.lock(self.donor).parent / "crates/rca-core/benches/composition.rs",
                                   self.donor/self.owned),
                     {"reason":"build-finished","success": True}]

    @staticmethod
    def lock(target):
        return target.parent / ("source-" + target.name) / "Cargo.lock"

    @staticmethod
    def artifact(package, name, src, output):
        bench = name == "composition"
        return {"reason":"compiler-artifact","package_id":package,"features":[],
                "target":{"name":name,"kind":["bench" if bench else "lib"],"src_path":str(src)},
                "filenames":[str(output)], "executable":str(output) if bench else None,"fresh":False}

    def report(self, rows):
        self.log.write_text("".join(json.dumps(r)+"\n" for r in rows), encoding="utf-8")

    def prepare(self):
        self.report(self.rows)
        return cache.prepare_seed(self.donor, self.log, self.home, lock_sha256=self.lock_sha)

    def copy(self, seed):
        return cache.seed_benchmark(seed, self.target, lock_sha256=self.lock_sha)

    def test_copies_only_reported_registry_units_with_independent_files(self):
        unreported = self.donor / "release/build/ordinary/aaaaaaaaaaaaaaaa/rogue"
        unreported.parent.mkdir(parents=True)
        unreported.touch()
        allowed = self.copy(self.prepare())
        self.assertTrue(allowed)
        self.assertFalse((self.target/self.owned).exists())
        self.assertFalse((self.target/unreported.relative_to(self.donor)).exists())
        self.assertTrue((self.target/self.unit/"fingerprint/lib-ordinary").is_file())
        (self.target/self.compiled).write_bytes(b"changed by the recipient")
        self.assertEqual((self.donor/self.compiled).read_bytes(), b"original")

    def test_regular_flat_cargo_layout_falls_back_before_writing(self):
        flat = self.donor / "release/deps/libordinary.rlib"
        flat.parent.mkdir()
        flat.touch()
        self.rows[0]["filenames"] = [str(flat)]
        self.assertFalse(self.copy(self.prepare()))
        self.assertFalse(self.target.exists())

    def test_reported_file_outside_donor_cannot_trigger_fallback(self):
        self.rows[0]["filenames"] = [str(self.archive)]
        with self.assertRaises(ValueError):
            self.prepare()

    def test_modified_registry_download_or_role_lock_is_not_a_cache_miss(self):
        original = self.archive.read_bytes()
        self.archive.write_bytes(b"changed")
        with self.assertRaises(ValueError):
            self.prepare()
        self.archive.write_bytes(original)
        seed = self.prepare()
        self.lock(self.target).write_bytes(b"changed")
        with self.assertRaises(ValueError):
            self.copy(seed)

    def test_modified_or_extra_unpacked_registry_source_is_rejected(self):
        original = self.src.read_bytes()
        self.src.write_bytes(b"changed after download")
        with self.assertRaises(ValueError):
            self.prepare()
        self.src.write_bytes(original)
        self.src.with_name("extra.rs").write_bytes(b"not in the authenticated archive")
        with self.assertRaises(ValueError):
            self.prepare()

    def test_redirected_unit_or_mutated_donor_is_rejected(self):
        seed = self.prepare()
        (self.donor/self.compiled).write_bytes(b"changed")
        with self.assertRaises(ValueError):
            self.copy(seed)
        (self.donor/self.compiled).unlink()
        (self.donor/self.compiled).symlink_to(self.archive)
        with self.assertRaises(ValueError):
            self.prepare()

    def test_never_seeds_cold_compile_target_or_existing_target(self):
        seed = self.prepare()
        wrong = self.target.parent / "compile-direct-01-subject"
        with self.assertRaises(ValueError):
            cache.seed_benchmark(seed, wrong, lock_sha256=self.lock_sha)
        self.target.mkdir()
        with self.assertRaises(ValueError):
            self.copy(seed)

    def test_reused_path_git_or_unseeded_registry_unit_fails(self):
        allowed = self.copy(self.prepare())
        reg = self.artifact(self.identity, "ordinary", self.src, self.target/self.compiled)
        reg["fresh"] = True
        bench = self.artifact(self.path_id,"composition",Path("workload"),self.target/self.owned)
        finished = {"reason":"build-finished","success":True}
        self.report([reg,bench,finished])
        cache.verify_rebuild(self.log, allowed)
        for extra in ({**reg, "package_id":self.path_id}, {**reg, "package_id":"git+https://somewhere#other"},
                      {**reg, "filenames":[str(self.target/"release/build/ordinary/1111111111111111/out/lib.rlib")]},
                      {**bench,"fresh":True}, {**bench,"fresh":None}):
            with self.subTest(extra=extra):
                self.report([reg, extra, bench, finished])
                with self.assertRaises(ValueError):
                    cache.verify_rebuild(self.log, allowed)

    def test_partial_or_failed_reports_never_qualify(self):
        for rows in (self.rows[:-1], [self.rows[0], self.rows[-1]],
                     [*self.rows[:-1], {"reason":"build-finished","success":False}]):
            with self.subTest(rows=rows):
                self.report(rows)
                with self.assertRaises(ValueError):
                    cache.verify_rebuild(self.log, {})

    def test_reported_fresh_file_must_match_the_sealed_copy(self):
        allowed = self.copy(self.prepare())
        compiled = self.target / self.compiled
        counterfeit = compiled.with_name("counterfeit.rlib")
        counterfeit.write_bytes(b"injected in the correct unit")
        for file in (counterfeit, compiled):
            file.write_bytes(b"not the sealed bytes")
            reg = self.artifact(self.identity, "ordinary", self.src, file)
            self.report([{**reg, "fresh": True}, *self.rows[1:]])
            with self.subTest(file=file), self.assertRaises(ValueError):
                cache.verify_rebuild(self.log, allowed)

    def native_fixture(self):
        build = self.root / "native-build"
        build.mkdir(mode=0o700)
        donor_role, recipient_role = "candidate-native", "reference-deterministic"
        donor, recipient = [build / ("target-" + role) for role in (donor_role, recipient_role)]
        shutil.copytree(self.donor, donor)
        for role in (donor_role, recipient_role):
            source = build / role
            source.mkdir()
            shutil.copy2(self.lock(self.donor), source / "Cargo.lock")
            (source / "Cargo.toml").touch()
            (source / "qualification.rs").touch()

        def transcript(role, registry_fresh):
            source, target = build / role, build / ("target-" + role)
            registry = self.artifact(self.identity, "ordinary", self.src, target / self.compiled)
            bench = self.artifact(self.path_id, "qualification", source / "qualification.rs", target / self.owned)
            bench.update(manifest_path=str(source / "Cargo.toml"), executable=str(target / self.owned))
            bench["target"]["kind"] = ["bench"]
            rows = [{**registry, "fresh":registry_fresh}, bench, {"reason":"build-finished","success":True}]
            log = build / (role + "-normal-build.jsonl")
            log.write_text("".join(json.dumps(r)+"\n" for r in rows), encoding="utf-8")
            return rows, log

        transcript(donor_role, False)
        return build, donor, recipient, recipient_role, transcript

    def test_native_role_must_rebuild_its_own_qualification_binary(self):
        build, donor, recipient, role, transcript = self.native_fixture()
        seed = cache.prepare_native_seed(build, self.home, lock_sha256=self.lock_sha)
        allowed = cache.seed_native_benchmark(seed, build, role, lock_sha256=self.lock_sha)
        self.assertFalse((recipient / self.owned).exists())
        (recipient / self.owned).parent.mkdir(parents=True)
        (recipient / self.owned).write_bytes(b"separate benchmark")
        rows, log = transcript(role, True)
        self.assertEqual(cache.verify_native_rebuild(build, role, allowed), 1)
        for bad in ({"fresh":True}, {"manifest_path":str(build/"candidate-native/Cargo.toml")},
                    {"filenames":[str(donor/self.owned)]}, {"executable":str(donor/self.owned)}):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                log.write_text("".join(json.dumps(r)+"\n" for r in [rows[0], {**rows[1], **bad}, rows[2]]))
                cache.verify_native_rebuild(build, role, allowed)

    def test_native_seed_rejects_wrong_lock_capture_and_timed_target(self):
        build, _, recipient, role, _ = self.native_fixture()
        with self.assertRaises(ValueError):
            cache.prepare_native_seed(build, self.home, lock_sha256="0"*64)
        seed = cache.prepare_native_seed(build, self.home, lock_sha256=self.lock_sha)
        for invalid in ("compile-direct-01-subject", "candidate-native", "../escape", "reference-native-allocation"):
            with self.subTest(role=invalid), self.assertRaises(ValueError):
                cache.seed_native_benchmark(seed, build, invalid, lock_sha256=self.lock_sha)
        with self.assertRaises(ValueError):
            cache.seed_native_benchmark(seed, build, role, lock_sha256="0"*64)
        second = self.root / "another-capture"
        shutil.copytree(build, second)
        with self.assertRaises(ValueError):
            cache.seed_native_benchmark(seed, second, role, lock_sha256=self.lock_sha)
        self.assertFalse(recipient.exists())
