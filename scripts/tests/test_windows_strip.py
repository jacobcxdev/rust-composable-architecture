"""Reject unpinned stripping tools and changes outside PE debug data."""

import importlib.util
import hashlib
import json
from pathlib import Path
import struct
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))


def image():
    """Use literal, disjoint code, debug and relocation ranges."""
    data = bytearray(2048)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 0x80)
    data[0x80:0x84] = b"PE\0\0"
    struct.pack_into("<HHIIIHH", data, 0x84, 0x8664, 3, 0, 0, 0, 240, 2)
    struct.pack_into("<H", data, 0x98, 0x20B)
    struct.pack_into("<II", data, 0x98 + 32, 4096, 512)
    struct.pack_into("<I", data, 0x98 + 108, 16)
    struct.pack_into("<II", data, 0x98 + 112 + 5 * 8, 0x3000, 12)
    struct.pack_into("<II", data, 0x98 + 112 + 6 * 8, 0x2000, 28)
    for index, (name, virtual_size, rva, offset, flags) in enumerate((
        (b".text", 480, 0x1000, 512, 0x60000020),
        (b".rdata", 256, 0x2000, 1024, 0x40000040),
        (b".reloc", 12, 0x3000, 1536, 0x42000040),
    )):
        struct.pack_into("<8sIIIIIIHHI", data, 0x188 + index * 40,
                         name, virtual_size, rva, 512, offset, 0, 0, 0, 0, flags)
    data[512:1024] = b"\xCC" * 512
    struct.pack_into("<IIHHIIII", data, 1024, 0, 0, 0, 0, 2, 32, 0x2040, 1088)
    data[1088:1120] = b"RSDS" + b"D" * 28
    data[1200:1216] = b"ordinary data!!!"
    data[1536:1548] = b"relocations!"
    return bytes(data)


def cleared(data):
    result = bytearray(data)
    result[0x138:0x140] = bytes(8)
    result[1024:1052] = bytes(28)
    result[1088:1120] = bytes(32)
    result[992:1024] = bytes(32)
    return bytes(result)


def tool_fixture(root):
    """Stand in for the four Windows-only tool files in capture tests."""
    tools = {name: str(root / f"{name}.fixture")
             for name in ("python", "lief_wheel", "lief_init", "lief_native")}
    for name, path in tools.items():
        Path(path).write_text(name, encoding="utf-8")
    return tools


def native_transform(command, log, tools, *, leave_debug=False):
    """Replace only the unavailable Windows process, retaining real PE bytes."""
    import windows_strip

    if command[1:4] != ["-I", "-S", str(Path(windows_strip.__file__).resolve())]:
        raise AssertionError("transform did not use the isolated shared helper")
    intermediate = Path(command[command.index("--input") + 1])
    output = Path(command[command.index("--output") + 1])
    before = intermediate.read_bytes()
    if before != image():
        raise AssertionError("unexpected LLVM intermediate")
    output.write_bytes(before if leave_debug else cleared(before))
    record = {
        "intermediate": windows_strip.file_record(intermediate),
        "stripped": windows_strip.file_record(output),
        "tools": {name: windows_strip.file_record(path) for name, path in tools.items()},
        "configuration": windows_strip.CONFIGURATION,
        "lief_version": "1.0.0-d05b3499b", "python_version": "fixture",
        "verification": {"mapped_executable_bytes_identical": True, "nondebug_bytes_identical": True,
                         "padding_changed_bytes": 32, "changed_bytes": 72},
    }
    output.with_suffix(".strip.json").write_text(json.dumps(record), encoding="utf-8")
    log.write_text(json.dumps(record), encoding="utf-8")


class WindowsStripTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.find_spec("windows_strip")
        self.assertIsNotNone(spec, "shared Windows debug stripping is not implemented")
        self.strip = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.strip)

    def test_debug_removal_and_file_padding_preserve_all_mapped_code(self):
        before = image()
        result = self.strip.validate_transform(before, cleared(before))
        self.assertEqual(result["padding_changed_bytes"], 32)
        self.assertEqual(result["mapped_executable_bytes_identical"], True)

    def test_changed_code_data_layout_or_nonzero_debug_is_rejected(self):
        before = image()
        for offset in (512, 991, 1200, 1536, 0x188 + 12, 0x128, 0x138, 1088):
            with self.subTest(offset=offset):
                after = bytearray(cleared(before))
                after[offset] ^= 1
                with self.assertRaises(ValueError):
                    self.strip.validate_transform(before, bytes(after))

    def test_debug_payload_cannot_authorise_code_or_relocation_changes(self):
        for rva, offset in ((0x1000, 512), (0x3000, 1536)):
            with self.subTest(rva=rva):
                before = bytearray(image())
                struct.pack_into("<III", before, 1024 + 16, 12, rva, offset)
                after = bytearray(cleared(before))
                after[offset:offset + 12] = bytes(12)
                with self.assertRaises(ValueError):
                    self.strip.validate_transform(bytes(before), bytes(after))

    def test_truncated_overlapping_and_misdirected_inputs_are_rejected(self):
        malformed = [image()[:100], image()[:-1]]
        for offset, value in ((0x188 + 40 + 20, 512), (1048, 1500), (1040, 2**32 - 1)):
            data = bytearray(image())
            struct.pack_into("<I", data, offset, value)
            malformed.append(bytes(data))
        for before in malformed:
            with self.subTest(length=len(before)), self.assertRaises(ValueError):
                self.strip.validate_transform(before, cleared(before))

    def test_preflight_requires_explicit_pinned_wheel_before_import(self):
        with self.assertRaisesRegex(ValueError, "LIEF"):
            self.strip.preflight({})
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            wheel = root / "lief-1.0.0-cp312-abi3-win_amd64.whl"
            wheel.write_bytes(b"untrusted wheel")
            with patch.object(sys, "platform", "win32"), \
                    patch.object(self.strip.sysconfig, "get_platform", return_value="win-amd64"), \
                    patch.object(self.strip.sysconfig, "get_config_var", return_value=0), \
                    self.assertRaisesRegex(ValueError, "wheel.*hash"):
                self.strip.preflight({"RCA_WINDOWS_LIEF_WHEEL": str(wheel),
                                      "RCA_WINDOWS_LIEF_SITE": str(root)})

    def test_preflight_rejects_mutated_payloads_extra_code_and_links(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            wheel = root / self.strip.WHEEL_NAME
            wheel.write_bytes(b"wheel")
            package = root / "lief"
            package.mkdir()
            payloads = {}
            for name, filename in (("lief_init", "__init__.py"), ("lief_native", "_lief.pyd")):
                (package / filename).write_bytes(name.encode())
                payloads[name] = (f"lief/{filename}", hashlib.sha256(name.encode()).hexdigest())
            env = {"RCA_WINDOWS_LIEF_WHEEL": str(wheel), "RCA_WINDOWS_LIEF_SITE": str(root)}
            with patch.object(sys, "platform", "win32"), \
                    patch.object(self.strip.sysconfig, "get_platform", return_value="win-amd64"), \
                    patch.object(self.strip.sysconfig, "get_config_var", return_value=0), \
                    patch.object(self.strip, "WHEEL_SHA256", hashlib.sha256(b"wheel").hexdigest()), \
                    patch.object(self.strip, "PAYLOADS", payloads):
                self.assertEqual(Path(self.strip.preflight(env)["lief_native"]), package / "_lief.pyd")
                for name, (relative, _) in payloads.items():
                    path = root / relative
                    path.write_bytes(b"changed")
                    with self.subTest(name=name), self.assertRaisesRegex(ValueError, "hash"):
                        self.strip.preflight(env)
                    path.write_bytes(name.encode())
                for filename in ("injected.py", "_lief.pyc", "other.dll"):
                    extra = package / filename
                    extra.write_bytes(b"code")
                    with self.subTest(filename=filename), self.assertRaisesRegex(ValueError, "unpinned"):
                        self.strip.preflight(env)
                    extra.unlink()
                link = root / "wheel-link"
                link.symlink_to(wheel)
                with self.assertRaises(ValueError):
                    self.strip.preflight({**env, "RCA_WINDOWS_LIEF_WHEEL": str(link)})

    def test_unsupported_platform_and_interpreter_fail_before_tool_use(self):
        env = {"RCA_WINDOWS_LIEF_WHEEL": "/missing", "RCA_WINDOWS_LIEF_SITE": "/missing"}
        for platform, version, free_threaded, architecture in (
            ("darwin", (3, 14), 0, "macosx"), ("win32", (3, 11), 0, "win-amd64"),
            ("win32", (3, 14), 1, "win-amd64"), ("win32", (3, 14), 0, "win-arm64"),
            ("win32", (4, 0), 0, "win-amd64"),
        ):
            with self.subTest(platform=platform, version=version, architecture=architecture), \
                    patch.object(sys, "platform", platform), patch.object(sys, "version_info", version), \
                    patch.object(self.strip.sysconfig, "get_platform", return_value=architecture), \
                    patch.object(self.strip.sysconfig, "get_config_var", return_value=free_threaded), \
                    self.assertRaisesRegex(ValueError, "requires standard x64"):
                self.strip.preflight(env)

    def test_existing_lief_submodule_is_rejected_before_import(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tools = tool_fixture(root)
            with patch.object(self.strip, "preflight", return_value=tools), \
                    patch.dict(sys.modules, {"lief.injected": object()}), \
                    patch.object(self.strip.importlib, "import_module") as imported, \
                    self.assertRaisesRegex(ValueError, "import state"):
                self.strip.transform(root / "input.exe", root / "output.exe", tools)
            imported.assert_not_called()

    def test_lief_writer_is_bounded_and_its_output_is_independently_checked(self):
        for fault in (None, "version", "native", "loader", "code", "tool"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                tools = tool_fixture(root)
                intermediate, output = root / "llvm.exe", root / "stripped.exe"
                intermediate.write_bytes(image())
                config = SimpleNamespace()
                binary = SimpleNamespace(imports=["kernel32:ExitProcess"], delay_imports=[],
                                         relocations=["0x3000"], clear_debug=lambda: True)
                final = SimpleNamespace(imports=["wrong"] if fault == "loader" else binary.imports,
                                        delay_imports=[], relocations=["0x3000"], has_debug=False,
                                        debug_dir=SimpleNamespace(rva=0, size=0))

                def write(path, actual_config):
                    self.assertIs(actual_config, config)
                    self.assertEqual(vars(config), {
                        "imports": False, "exports": False, "resources": False, "relocations": False,
                        "load_configuration": False, "tls": False, "overlay": True, "debug": True,
                        "dos_stub": True, "force_relocating": False,
                    })
                    result = bytearray(cleared(image()))
                    if fault == "code":
                        result[512] = 0
                    if fault == "tool":
                        Path(tools["lief_native"]).write_bytes(b"changed")
                    Path(path).write_bytes(result)

                binary.write = write
                lief = SimpleNamespace(
                    __version__="wrong" if fault == "version" else "1.0.0-d05b3499b",
                    __file__=tools["lief_init"],
                    _lief=SimpleNamespace(__file__="wrong" if fault == "native" else tools["lief_native"]),
                    PE=SimpleNamespace(parse=lambda path: binary if Path(path) == intermediate else final,
                                       Builder=SimpleNamespace(config_t=lambda: config)),
                )
                with patch.object(self.strip, "preflight", return_value=tools), \
                        patch.object(self.strip.importlib, "import_module", return_value=lief), \
                        patch.object(sys, "path", list(sys.path)), patch.object(sys, "dont_write_bytecode", True):
                    if fault:
                        with self.assertRaises(ValueError):
                            self.strip.transform(intermediate, output, tools)
                    else:
                        result = self.strip.transform(intermediate, output, tools)
                        self.assertEqual(result["stripped"]["sha256"], hashlib.sha256(cleared(image())).hexdigest())
                        self.assertEqual(result["verification"]["changed_bytes"], 72)
                self.assertEqual(intermediate.read_bytes(), image())

    def test_transform_preserves_existing_output_before_import(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tools = tool_fixture(root)
            intermediate, output = root / "llvm.exe", root / "stripped.exe"
            intermediate.write_bytes(image())
            output.write_bytes(b"retained evidence")
            with patch.object(self.strip, "preflight", return_value=tools), \
                    patch.object(self.strip.importlib, "import_module") as imported, \
                    self.assertRaisesRegex(ValueError, "already exists"):
                self.strip.transform(intermediate, output, tools)
            imported.assert_not_called()
            self.assertEqual(output.read_bytes(), b"retained evidence")

    def test_guarded_child_receipt_must_match_real_bytes_tools_and_configuration(self):
        for fault in ("tools", "configuration", "intermediate", "output"):
            with self.subTest(fault=fault), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                tools = tool_fixture(root)
                intermediate, output = root / "llvm.exe", root / "stripped.exe"
                intermediate.write_bytes(image())

                def run(command, log, cwd, env):
                    native_transform(command, log, tools)
                    receipt = output.with_suffix(".strip.json")
                    result = json.loads(receipt.read_text(encoding="utf-8"))
                    if fault == "tools":
                        result["tools"]["lief_native"]["sha256"] = "0" * 64
                    elif fault == "configuration":
                        result["configuration"]["imports"] = True
                    elif fault == "intermediate":
                        result["intermediate"]["sha256"] = "0" * 64
                    else:
                        changed = bytearray(output.read_bytes())
                        changed[512] = 0
                        output.write_bytes(changed)
                    receipt.write_text(json.dumps(result), encoding="utf-8")

                with self.assertRaisesRegex(ValueError, "receipt differs"):
                    self.strip.finish_strip(intermediate, output, root / "strip.log", root, {},
                                            SimpleNamespace(run=run), tools)


if __name__ == "__main__":
    unittest.main()
