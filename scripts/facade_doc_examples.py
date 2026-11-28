"""Generate independent downstream doctest packages from examples that use the RCA facade."""

import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re
import tomllib


ROOT = Path(__file__).resolve().parents[1]
CRATE_ROOT = ROOT / "crates"
CRATES = ("rca", "rca-core", "rca-test", "rca-tokio", "rca-macros", "rca-persistence", "rust-composable-architecture")
DOC = re.compile(r"^\s*//[/!] ?(.*)$")
SHIM = re.compile(r"# (?:use rca_(?:core|test|tokio) as rca;|mod rca \{ "
                  r"(?:pub use (?:::rca|rca_(?:core|test|macros))::[\w{},* :]+; )+\})")


def fences(path):
    """Read doc comments and the raw Markdown supplied to test_store_api! or doc attributes."""
    opening, raw = None, None
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        doc = DOC.match(line)
        if raw is not None:
            if line.strip().rstrip(",") == '"' + raw:
                raw = None
                continue
        elif doc:
            line = doc[1]
        elif (match := re.fullmatch(r'\s*(doc = )?r(\x23*)"', line)) and (
            match[1] or path.relative_to(CRATE_ROOT).as_posix() in
            {"rca-test/src/store.rs", "rca-test/src/store/local.rs"}
        ):
            raw = match[2]
            continue
        else:
            continue
        if opening is None:
            if line.startswith("```"):
                opening, tag, lines = number, line[3:], []
        elif line == "```":
            yield opening, tag, "\n".join(lines)
            opening = None
        else:
            lines.append(line)
    if opening is not None:
        raise ValueError(f"{path}:{opening}: unterminated documentation example")


def consumer_body(body):
    """Remove only the defining crate's rustdoc alias or import-only stand-in module."""
    lines = []
    for line in body.splitlines():
        if SHIM.fullmatch(line):
            continue
        if line.startswith("# mod rca {"):
            raise ValueError(f"unrecognised facade shim: {line}")
        lines.append(line)
    return "\n".join(lines)


def runtime_example(body):
    """Return the portable example and, for selected functions, an executable test."""
    private_host = '/tests/support/mod.rs' in body
    if private_host:
        example, separator, main = body.rpartition("\n# fn main() {\n")
        if not separator or not all(line.startswith("# ") for line in main.splitlines()) or not main.endswith("# }"):
            raise ValueError("only a completely hidden, trailing fixture main can be replaced")
    else:
        example = body
    for name, result in (("route_actions", ""), ("take_product", ".unwrap()"), ("count_once", ".unwrap()")):
        if f"async fn {name}(runtime: Runtime)" in example:
            if private_host and f"{name}(host.runtime())" not in main:
                raise ValueError("private host does not run the documented function")
            return example, (example + "\n# fn main() {\n#     let host = rca::DeterministicRuntime::new();\n"
                             + f"#     futures_executor::block_on(host.run({name}(host.runtime()))).unwrap(){result};\n# }}")
    if private_host:
        raise ValueError("unrecognised private host in facade docs")
    return example, None


def requirements(path, body):
    """Use the documented owning API's feature; do not expose its implementation crate."""
    crate, _, relative = path.relative_to(CRATE_ROOT).as_posix().split("/", 2)
    features = set()
    if crate == "rca-macros":
        features.add("macros")
    if crate == "rca-tokio":
        features.add("tokio")
    if crate == "rca-test" and relative != "store/local.rs":
        features.add("test-util")
    if relative.startswith("diagnostics"):
        features.add("diagnostics")
    if relative in {"issue/collector.rs", "issue/stub.rs", "runtime/fixtures.rs"}:
        features.add("test-util")
    if re.search(r"\blocal::|\brca::local\b", body):
        features.add("local")
    if re.search(r"\brca::(?:\w+::)*DeterministicRuntime\b|\buse rca::\{[^}]*\bDeterministicRuntime\b", body):
        features.add("test-util")
    extra = []
    if "futures_executor::" in body:
        extra.append("executor")
    if "rca_test::" in body:
        if (crate, relative) != ("rca-test", "store/local.rs") or "rca_test::local::TestStore" not in body:
            raise ValueError(f"{path}: check and document the explicit rca-test dependency first")
        extra.append("direct-test-local")
    return tuple(sorted(features)), tuple(extra)


def generate(destination, registry=None):
    """Group by manifest, not all-features. Cargo must test each member separately."""
    groups, controls, counts = defaultdict(list), defaultdict(set), Counter()
    for crate in CRATES:
        for file in sorted((CRATE_ROOT / crate / "src").rglob("*.rs")):
            path = file.relative_to(ROOT)
            for line, tag, original in fences(file):
                body, hosted = runtime_example(consumer_body(original))
                if not re.search(r"\brca::", body):
                    counts["not a facade example"] += 1
                    continue
                if any(flag in {"ignore", "text", "toml"} for flag in tag.split(",")):
                    counts["illustrative, not executed"] += 1
                    continue
                if not re.fullmatch(r"(?:rust|no_run|compile_fail(?:,E\d{4})?)?", tag):
                    raise ValueError(f"{path}:{line}: classify Rust fence {tag!r}")
                counts["compile_fail" if "compile_fail" in tag else tag or "rust"] += 1
                features, extra = requirements(file, body)
                block = f"## {path}:{line}\n\n```{tag}\n{body}\n```\n"
                groups[features, extra, "rca"].append(block)
                if hosted:
                    # Typecheck the real app without features; run its assertions with a test host separately.
                    groups[("test-util",), ("executor",), "rca"].append(
                        f"## {path}:{line} with a public test host\n\n```\n{hosted}\n```\n")
                if tag.startswith("compile_fail"):
                    imports = tuple(re.findall(r"(?m)^(?:# )?(use rca::[^;]+;)", body))
                    if not imports:
                        raise ValueError(f"{path}:{line}: add a positive control for the required facade exports")
                    controls[features, extra, "rca"].add(imports)
                if features == ("macros",) and not extra:
                    # Exercise proc-macro package discovery without a compatibility alias.
                    groups[features, extra, "architecture"].append(re.sub(r"\brca::", "architecture::", block))
    if not groups or not counts["compile_fail"]:
        raise ValueError("expected facade examples with both positive and negative cases")
    destination.mkdir(parents=True, exist_ok=False)
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
    version = workspace["package"]["version"]

    def dependency(name):
        return f'version = "={version}", ' + (f'registry = "{registry}"' if registry else f'path = {json.dumps(str(CRATE_ROOT / name))}')

    for (features, extra, alias), blocks in sorted(groups.items()):
        parts = (["alias"] if alias != "rca" else []) + list(features) + list(extra)
        package = "consumer-" + "-".join(parts or ["minimal"])
        directory = destination / package
        (directory / "src").mkdir(parents=True)
        deps = [f'{alias} = {{ package = "rca", {dependency("rca")}, default-features = false, features = {json.dumps(features)} }}']
        if "tokio" in features:
            tokio = workspace["dependencies"]["tokio"]
            deps.append(f'tokio = {{ version = "={tokio["version"]}", features = ["rt", "time"] }}')
        dev = []
        if "executor" in extra:
            dev.append(f'futures-executor = "={workspace["dependencies"]["futures-executor"]["version"]}"')
        if "direct-test-local" in extra:
            dev.append(f'rca-test = {{ {dependency("rca-test")}, features = ["local"] }}')
        (directory / "Cargo.toml").write_text(
            f'[package]\nname = "{package}"\nversion = "0.0.0"\n'
            + f'edition = "{workspace["package"]["edition"]}"\nrust-version = "{workspace["package"]["rust-version"]}"\npublish = false\n'
            + '\n[dependencies]\n' + '\n'.join(deps) + '\n'
            + ('\n[dev-dependencies]\n' + '\n'.join(dev) + '\n' if dev else ''), encoding="utf-8")
        (directory / "src/lib.rs").write_text('#![doc = include_str!("../examples.md")]\n', encoding="utf-8")
        docs = "\n".join(blocks)
        if imports := controls[features, extra, alias]:
            # A missing export must not make a compile_fail example pass.
            docs += "\n## Imports for the rejection examples\n"
            for names in sorted(imports):
                docs += "\n```no_run\n" + "\n".join(names) + "\n```\n"
        (directory / "examples.md").write_text(docs, encoding="utf-8")
        print(f"{package}: {len(blocks)} examples")
    (destination / "Cargo.toml").write_text('[workspace]\nmembers = ["consumer-*"]\nresolver = "3"\n', encoding="utf-8")
    print("Source fences (additional controls and replays excluded):", dict(sorted(counts.items())))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path, help="new scratch directory for the generated workspace")
    parser.add_argument("--registry", choices=["rca-local"], help="use staged archives instead of the checkout")
    args = parser.parse_args()
    generate(args.destination, args.registry)
