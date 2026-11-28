"""Check committed compiler inputs without building the candidate."""

import io
import os
import posixpath
import re
import subprocess
import tempfile


def read_blobs(specs):
    """Yield (revision:path, bytes) pairs from one Git process."""
    specs = list(dict.fromkeys(specs))
    if not specs:
        return
    if any(b"\n" in spec for spec in specs):
        raise ValueError("Cannot frame a Git path containing a newline")
    # Git emits each blob before requesting the next. Do not let its output
    # pipe compete with a large input pipe (both can fill on macOS).
    with tempfile.TemporaryFile() as requests:
        requests.write(b"\n".join(specs) + b"\n")
        requests.seek(0)
        output = io.BytesIO(subprocess.check_output(["git", "cat-file", "--batch"], stdin=requests))
    for spec in specs:
        _oid, kind, size = output.readline().split()
        if kind != b"blob":
            raise ValueError("Expected a committed file")
        data = output.read(int(size))
        if output.read(1) != b"\n":
            raise ValueError("Incomplete Git blob")
        yield spec, data


_INCLUDE = re.compile(rb"\binclude(?:_str|_bytes)?\b")
# Recognise the two path forms in RCA. Unknown macro arguments require full CI.
_PATH = rb'"([^"\\\r\n]+)"'
_LITERAL = re.compile(rb"\s*!\s*\(\s*" + _PATH + rb"\s*,?\s*\)")
_MANIFEST = re.compile(rb'\s*!\s*\(\s*concat\s*!\s*\(\s*env\s*!\s*\(\s*"CARGO_MANIFEST_DIR"\s*\)\s*,\s*'
                       + _PATH + rb"\s*,?\s*\)\s*,?\s*\)")
_MACRO_CONTEXT = re.compile(rb"\s*(?:!|/\*|//(?![!/])|as\b|[,};])")


def has_markdown_include(head, paths):
    """Return true for a changed include or one whose input is unknown."""
    found = subprocess.run(["git", "grep", "-a", "-l", "-z", "-w", "-e", "include",
                            "-e", "include_str", "-e", "include_bytes", head,
                            "--", ":(glob)**/*.rs"], stdout=subprocess.PIPE)
    if found.returncode == 1:
        return False
    found.check_returncode()
    changed = {path.casefold() for path in paths}
    names = {posixpath.basename(path) for path in changed}
    for spec, source in read_blobs(found.stdout.rstrip(b"\0").split(b"\0")):
        parent = posixpath.dirname(os.fsdecode(spec.partition(b":")[2]))
        for call in _INCLUDE.finditer(source):
            end = call.end()
            if literal := _LITERAL.match(source, end):
                target = os.fsdecode(literal[1])
                if ":" in target:
                    return True
                target = posixpath.normpath(posixpath.join(parent, target))
                if posixpath.isabs(target) or target.startswith("../") or target.casefold() in changed:
                    return True
            elif manifest := _MANIFEST.match(source, end):
                # The package (or fixture) need not share the source directory.
                target = os.fsdecode(manifest[1])
                if not target.startswith("/") or posixpath.basename(target).casefold() in names:
                    return True
            elif call[0] != b"include" or _MACRO_CONTEXT.match(source, end):
                # "include" also occurs in prose. A macro, import or opaque
                # invocation can read a file without spelling its full name.
                return True
    return False
