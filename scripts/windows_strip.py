"""Remove embedded PE debug data after LLVM stripping, with pinned LIEF tools.

This helper never installs tools. Both native capture routes retain the LLVM
intermediate and use their existing strict validator on the final executable.
"""

import argparse
import hashlib
import importlib
import json
from pathlib import Path
import struct
import sys
import sysconfig


WHEEL_NAME = "lief-1.0.0-cp312-abi3-win_amd64.whl"
WHEEL_SHA256 = "ca7774b0d88f4528a1c153d7a9d2798800d6389847b966861fad9f2edb17a593"
LIEF_VERSION = "1.0.0-d05b3499b"
PAYLOADS = {
    "lief_init": ("lief/__init__.py", "e719144892ca2f03bf026a4c730e4a289458970d99369b257014bb0d195705d5"),
    "lief_native": ("lief/_lief.pyd", "b2008ef9dee5bcd925a619c2f4b771051faa22c0731ecf947b2277ef950f82ae"),
}
CONFIGURATION = {
    "imports": False, "exports": False, "resources": False, "relocations": False,
    "load_configuration": False, "tls": False, "overlay": True, "debug": True,
    "dos_stub": True, "force_relocating": False,
}


def file_record(path):
    """Require a nonempty regular file and retain its exact identity."""
    path = Path(path)
    if not path.is_absolute() or path.is_symlink() or not path.is_file() or not path.stat().st_size:
        raise ValueError(f"missing, empty or linked strip input: {path}")
    return {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "size_bytes": path.stat().st_size}


def preflight(env):
    """Check the explicit installation before importing any LIEF code."""
    if not env.get("RCA_WINDOWS_LIEF_WHEEL") or not env.get("RCA_WINDOWS_LIEF_SITE"):
        raise ValueError("set RCA_WINDOWS_LIEF_WHEEL and RCA_WINDOWS_LIEF_SITE for pinned LIEF")
    if (sys.platform != "win32" or sys.implementation.name != "cpython"
            or sys.version_info[0] != 3 or sys.version_info < (3, 12)
            or sysconfig.get_platform() != "win-amd64" or struct.calcsize("P") != 8
            or sysconfig.get_config_var("Py_GIL_DISABLED")):
        raise ValueError("LIEF stripping requires standard x64 Windows CPython 3.12 or later")
    wheel = Path(env["RCA_WINDOWS_LIEF_WHEEL"])
    if wheel.name != WHEEL_NAME or file_record(wheel)["sha256"] != WHEEL_SHA256:
        raise ValueError("LIEF wheel name or hash differs from the pinned release")
    site = Path(env["RCA_WINDOWS_LIEF_SITE"])
    if not site.is_absolute() or site.is_symlink() or not site.is_dir() or (site / "lief").is_symlink():
        raise ValueError("LIEF site must be an explicit directory without links")
    tools = {"python": str(Path(sys.executable).resolve(strict=True)), "lief_wheel": str(wheel)}
    for name, (relative, expected) in PAYLOADS.items():
        path = site / relative
        if file_record(path)["sha256"] != expected:
            raise ValueError(f"LIEF {name} hash differs from the pinned wheel")
        tools[name] = str(path)
    executable_files = {path.relative_to(site).as_posix() for path in (site / "lief").rglob("*")
                        if path.suffix.lower() in (".py", ".pyc", ".pyd", ".dll", ".pth")}
    if executable_files != {relative for relative, _ in PAYLOADS.values()}:
        raise ValueError("LIEF installation contains unpinned executable files")
    file_record(tools["python"])
    return tools


def _pe_ranges(data):
    """Locate bounded debug ranges without writing PE headers or payloads."""
    try:
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        machine, count, _, symbols, symbol_count, optional_size, flags = struct.unpack_from("<HHIIIHH", data, pe + 4)
        optional = pe + 24
        magic = struct.unpack_from("<H", data, optional)[0]
        alignment = struct.unpack_from("<I", data, optional + 36)[0]
        directory_count = struct.unpack_from("<I", data, optional + 108)[0]
        if (data[:2] != b"MZ" or data[pe:pe + 4] != b"PE\0\0" or machine != 0x8664
                or magic != 0x20B or symbols or symbol_count or not flags & 2 or not count
                or optional_size < 168 or not 7 <= directory_count <= (optional_size - 112) // 8
                or not 512 <= alignment <= 65536 or alignment & (alignment - 1)):
            raise ValueError("invalid symbol-free x64 PE input")
        table = optional + optional_size
        sections = []
        for index in range(count):
            _, virtual, rva, size, offset, _, _, _, _, characteristics = struct.unpack_from(
                "<8sIIIIIIHHI", data, table + index * 40)
            if size and (offset < table + count * 40 or offset % alignment or size % alignment
                         or offset + size > len(data)):
                raise ValueError("invalid PE section file range")
            sections.append((virtual, rva, size, offset, characteristics))
        file_ranges = sorted((offset, offset + size) for _, _, size, offset, _ in sections if size)
        if any(left[1] > right[0] for left, right in zip(file_ranges, file_ranges[1:])):
            raise ValueError("overlapping PE sections")

        def mapped(rva, size):
            matches = [(offset + rva - start, offset + rva - start + size)
                       for virtual, start, raw_size, offset, _ in sections
                       if start <= rva and rva + size <= start + min(virtual, raw_size)]
            if size <= 0 or len(matches) != 1:
                raise ValueError("unmapped or ambiguous PE data range")
            return matches[0]

        debug_entry = optional + 112 + 6 * 8
        debug_rva, debug_size = struct.unpack_from("<II", data, debug_entry)
        debug = [(debug_entry, debug_entry + 8)]
        if bool(debug_rva) != bool(debug_size) or debug_size % 28:
            raise ValueError("invalid PE debug directory")
        if debug_size:
            start, end = mapped(debug_rva, debug_size)
            debug.append((start, end))
            for entry in range(start, end, 28):
                size, rva, offset = struct.unpack_from("<III", data, entry + 16)
                if size:
                    payload = mapped(rva, size)
                    if payload[0] != offset:
                        raise ValueError("PE debug payload file address differs from its RVA")
                    debug.append(payload)
        protected = [(offset, offset + min(virtual, size))
                     for virtual, _, size, offset, flags in sections if flags & 0x20000000]
        for index in range(directory_count):
            rva, size = struct.unpack_from("<II", data, optional + 112 + index * 8)
            if index != 6 and size:
                protected.append((rva, rva + size) if index == 4 else mapped(rva, size))
        if any(start < high and low < end for start, end in debug for low, high in protected):
            raise ValueError("PE debug data overlaps executable or loader data")
        padding = [(offset + virtual, offset + size) for virtual, _, size, offset, _ in sections
                   if 0 < virtual < size]
        return debug, padding
    except struct.error as error:
        raise ValueError("truncated PE input") from error


def validate_transform(before, after):
    """Allow only debug removal and zeroing outside each section's VirtualSize."""
    if len(before) != len(after):
        raise ValueError("PE file size changed during debug removal")
    debug, padding = _pe_ranges(before)
    expected = bytearray(before)
    for start, end in debug:
        if any(after[start:end]):
            raise ValueError("PE debug data was not fully removed")
        expected[start:end] = bytes(end - start)
    padding_changed = 0
    for start, end in padding:
        for offset in range(start, end):
            if before[offset] != after[offset]:
                if after[offset] != 0:
                    raise ValueError("PE file-alignment padding changed to nonzero bytes")
                expected[offset] = 0
                padding_changed += 1
    if expected != after:
        raise ValueError("PE non-debug bytes, mapped code or section layout changed")
    return {"mapped_executable_bytes_identical": True, "nondebug_bytes_identical": True,
            "padding_changed_bytes": padding_changed,
            "changed_bytes": sum(left != right for left, right in zip(before, after))}


def transform(intermediate, output, tools):
    """Use the maintained LIEF writer; never patch executable bytes ourselves."""
    checked = preflight({"RCA_WINDOWS_LIEF_WHEEL": tools["lief_wheel"],
                         "RCA_WINDOWS_LIEF_SITE": str(Path(tools["lief_init"]).parent.parent)})
    if (checked != {name: tools[name] for name in checked}
            or any(name == "lief" or name.startswith("lief.") for name in sys.modules)):
        raise ValueError("LIEF tool identity or import state changed")
    identities = {name: file_record(path) for name, path in checked.items()}
    original = file_record(intermediate)
    if output.exists() or output.is_symlink():
        raise ValueError("stripped PE output already exists")
    before = intermediate.read_bytes()
    _pe_ranges(before)
    sys.dont_write_bytecode = True
    sys.path.insert(0, str(Path(checked["lief_init"]).parent.parent))
    lief = importlib.import_module("lief")
    if (lief.__version__ != LIEF_VERSION or Path(lief.__file__) != Path(checked["lief_init"])
            or Path(lief._lief.__file__) != Path(checked["lief_native"])):
        raise ValueError("loaded LIEF release identity differs")
    binary = lief.PE.parse(str(intermediate))
    if binary is None:
        raise ValueError("LIEF could not parse the PE input")
    loader_before = [[str(item) for item in getattr(binary, name)]
                     for name in ("imports", "delay_imports", "relocations")]
    if not binary.clear_debug():
        raise ValueError("LIEF could not clear the debug directory")
    config = lief.PE.Builder.config_t()
    for name, value in CONFIGURATION.items():
        setattr(config, name, value)
    binary.write(str(output), config)
    verification = validate_transform(before, output.read_bytes())
    result = lief.PE.parse(str(output))
    if (result is None or result.has_debug or result.debug_dir.rva or result.debug_dir.size
            or loader_before != [[str(item) for item in getattr(result, name)]
                                 for name in ("imports", "delay_imports", "relocations")]):
        raise ValueError("stripped PE debug or loader tables differ")
    if file_record(intermediate) != original or any(file_record(row["path"]) != row for row in identities.values()):
        raise ValueError("strip input or tool changed during transformation")
    return {"intermediate": original, "stripped": file_record(output), "tools": identities,
            "lief_version": lief.__version__, "python_version": sys.version,
            "configuration": CONFIGURATION, "verification": verification}


def finish_strip(intermediate, output, log, cwd, env, monitor, tools):
    """Run the shared transform in the capture's existing guarded child owner."""
    script = Path(__file__).resolve()
    command = [tools["python"], "-I", "-S", str(script), "--wheel", tools["lief_wheel"],
               "--site", str(Path(tools["lief_init"]).parent.parent),
               "--input", str(intermediate), "--output", str(output)]
    monitor.run(command, log, cwd, env)
    receipt = output.with_suffix(".strip.json")
    result = json.loads(receipt.read_text(encoding="utf-8"))
    if (result["intermediate"] != file_record(intermediate) or result["stripped"] != file_record(output)
            or result["tools"] != {name: file_record(tools[name]) for name in ("python", "lief_wheel", *PAYLOADS)}
            or result["configuration"] != CONFIGURATION or result["lief_version"] != LIEF_VERSION
            or result["verification"] != validate_transform(intermediate.read_bytes(), output.read_bytes())):
        raise ValueError("stripped PE receipt differs from retained files or tools")
    return {**result, "receipt": file_record(receipt), "log": file_record(log),
            "helper": file_record(script), "command": command}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("wheel", "site", "input", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    tools = preflight({"RCA_WINDOWS_LIEF_WHEEL": str(args.wheel), "RCA_WINDOWS_LIEF_SITE": str(args.site)})
    result = transform(args.input, args.output, tools)
    with args.output.with_suffix(".strip.json").open("x", encoding="utf-8") as receipt:
        json.dump(result, receipt, indent=2)
    print(json.dumps({"stripped": result["stripped"], "verification": result["verification"]}))


if __name__ == "__main__":
    main()
