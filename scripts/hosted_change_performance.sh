#!/usr/bin/env bash
# Linux hosted feedback. Native timing and release acceptance use separate commands.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
if [[ ${1:-} == release-base || ${1:-} == check-release-baseline ]]; then
    repository=${GITHUB_REPOSITORY:-jacobcxdev/rust-composable-architecture}
    tag=$(gh release list --repo "$repository" --exclude-drafts --exclude-pre-releases --limit 1 --json tagName --jq '.[0].tagName // ""')
    if [[ -z "$tag" ]]; then
        release=ab99e19ca772086f11e0a855f840f5cf9fae5d7f
    else
        [[ "$tag" =~ ^v?[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "Unexpected stable release tag: $tag" >&2; exit 1; }
        remote_object=$(gh api "repos/$repository/git/ref/tags/$tag" --jq .object.sha)
        local_object=$(git rev-parse --verify "refs/tags/$tag")
        [[ "$local_object" == "$remote_object" ]] || { echo "Local tag differs from the published release: $tag" >&2; exit 1; }
        release=$(git rev-parse --verify "refs/tags/$tag^{commit}")
    fi
    git cat-file -e "$release^{commit}"
    if [[ $1 == check-release-baseline ]]; then
        python3 -B - "$release" <<'PY'
import json
from pathlib import Path
import sys

sys.path.insert(0, "scripts")
import composition_runner as runner

pins = {
    "native": Path("docs/performance/baseline-revision.txt").read_text().strip(),
    "composition validator": runner.evidence.RELEASE_BASELINE_REVISION,
}
for path in (runner.CURRENT_PROTOCOL, runner.WINDOWS_PROTOCOL, runner.WINDOWS_COMPLETE_POLICY):
    pins[path.name] = json.loads(path.read_bytes())["reference_revision"]
for name, pinned in pins.items():
    if pinned != sys.argv[1]:
        sys.exit(f"{name} uses {pinned}, not the published baseline {sys.argv[1]}. Review the reference before qualification.")
PY
    fi
    echo "$release"
    exit 0
fi

fixture=tests/performance-profile
result="${RUNNER_TEMP:?}/performance-feedback"
base_source="$RUNNER_TEMP/performance-base"

case "${1:-}" in
    select)
        [[ "$PERF_HEAD" =~ ^[0-9a-f]{40}$ && "$PERF_RELEASE" =~ ^[0-9a-f]{40}$ ]]
        test "$(git rev-parse HEAD)" = "$PERF_HEAD"
        mkdir "$result"
        changed_from="$PERF_RELEASE"
        comparison="$PERF_RELEASE"
        if [[ -n "${PERF_BASE_TIP:-}" ]]; then
            [[ "$PERF_BASE_TIP" =~ ^[0-9a-f]{40}$ ]]
            changed_from=$(git merge-base "$PERF_BASE_TIP" "$PERF_HEAD")
            # The initial pull request starts from a metadata-only branch.
            if git cat-file -e "$changed_from:Cargo.toml" 2>/dev/null; then
                comparison="$changed_from"
            fi
        elif [[ "${PERF_PREVIOUS:-}" =~ ^[0-9a-f]{40}$ ]] &&
            git merge-base --is-ancestor "$PERF_PREVIOUS" "$PERF_HEAD" 2>/dev/null; then
            # Select what this push touched. Measure those workloads against
            # the release, so successive small changes do not reset the budget.
            changed_from="$PERF_PREVIOUS"
        fi
        git cat-file -e "$comparison^{commit}"
        git merge-base --is-ancestor "$comparison" "$PERF_HEAD"
        python3 scripts/performance_scope.py --base "$changed_from" --head "$PERF_HEAD" > "$result/scope.json"
        runtime=$(jq -er '[.families[] | select(. != "cold-build")] | join(",")' "$result/scope.json")
        echo "runtime=$runtime" >> "${GITHUB_OUTPUT:?}"
        jq -n --arg base "$comparison" --arg head "$PERF_HEAD" --arg range "$changed_from" \
            '{comparison:$base,candidate:$head,changed_from:$range}' > "$result/revisions.json"
        jq -r '"Selected families: " + (.families | join(", ")) + ". Selection is not a performance result."' "$result/scope.json" >> "${GITHUB_STEP_SUMMARY:?}"
        if jq -e '.families | index("cold-build") != null' "$result/scope.json" >/dev/null; then
            echo 'Cold builds were selected, not timed here. Compile-time claims need the guarded H1 check.' >> "$GITHUB_STEP_SUMMARY"
        fi
        ;;
    prepare)
        comparison=$(jq -er '.comparison' "$result/revisions.json")
        mkdir "$base_source"
        archive=$(mktemp "$RUNNER_TEMP/performance-src.XXXXXX")
        trap 'rm -f "$archive"' EXIT
        git archive --format=tar --output="$archive" "$comparison" -- . ":(exclude)$fixture/src"
        tar -xf "$archive" -C "$base_source"
        # The profiler is the common downstream consumer, not the product.
        git archive --format=tar --output="$archive" HEAD -- "$fixture/src" "$fixture/Cargo.toml"
        tar -xf "$archive" -C "$base_source"
        diff -qr "$fixture/src" "$base_source/$fixture/src"
        # Apply the consumer's dependency list without importing a single
        # dependency version from the candidate into the baseline.
        python3 - "$fixture/Cargo.lock" "$base_source/$fixture/Cargo.lock" <<'PY'
from pathlib import Path
import sys
import tomllib

def consumer(parts):
    (index,) = (i for i, part in enumerate(parts[1:], 1)
                if tomllib.loads(part)["name"] == "rca-performance-profile")
    return index

candidate, baseline = map(Path, sys.argv[1:])
separator = "\n[[package]]\n"
original = baseline.read_text().split(separator)
current = candidate.read_text().split(separator)
original[consumer(original)] = current[consumer(current)]
baseline.write_text(separator.join(original))
PY
        git rev-parse "$comparison:$fixture/Cargo.lock" "HEAD:$fixture/Cargo.lock" \
            "HEAD:$fixture/Cargo.toml" "HEAD:$fixture/src" > "$result/consumer-input-objects.txt"
        ;;
    measure)
        test "$(uname -s)" = Linux
        test "${PERF_RUNNER:-}" = github-hosted
        export RUSTC RUSTDOC
        export CARGO_INCREMENTAL=0
        RUSTC=$(rustup which --toolchain "${RUSTUP_TOOLCHAIN:?}" rustc)
        RUSTDOC=$(rustup which --toolchain "$RUSTUP_TOOLCHAIN" rustdoc)
        unset RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER
        "$RUSTC" -vV > "$result/rustc.txt"
        grep -Fx "release: $RUSTUP_TOOLCHAIN" "$result/rustc.txt" >/dev/null
        grep -Fx 'host: x86_64-unknown-linux-gnu' "$result/rustc.txt" >/dev/null
        cargo -Vv > "$result/cargo.txt"
        uname -a > "$result/host.txt"
        for role in baseline candidate; do
            source="$PWD"
            if [[ "$role" == baseline ]]; then source="$base_source"; fi
            consumer="$source/$fixture"
            export CARGO_TARGET_DIR="$consumer/target"
            (
                cd "$consumer"
                # Path dependencies outside this standalone workspace are product inputs too.
                cargo metadata --locked --format-version 1 --features deterministic,local,allocations > "$RUNNER_TEMP/change-metadata.json"
                jq -e '.resolve as $graph | $graph.nodes[] | select(.id == $graph.root) | .features | sort' \
                    "$RUNNER_TEMP/change-metadata.json" > "$result/$role-consumer-features.json"
                jq -er '[.packages[] | select(.source == null) | .name] | unique | .[]' \
                    "$RUNNER_TEMP/change-metadata.json" > "$result/$role-owned-packages.txt"
                clean=()
                while IFS= read -r package; do clean+=(-p "$package"); done < "$result/$role-owned-packages.txt"
                cargo clean --locked --release "${clean[@]}"
                cargo build --locked --release --features deterministic,local,allocations --bin rca-performance-profile
            )
            binary="$CARGO_TARGET_DIR/release/rca-performance-profile"
            sha256sum "$binary" "$consumer/Cargo.lock" > "$result/$role.sha256"
            if [[ "$role" == baseline ]]; then baseline_bin="$binary"; fi
        done
        cmp "$result/baseline-consumer-features.json" "$result/candidate-consumer-features.json"
        status=0
        python3 scripts/compare_change_resources.py --baseline-bin "$baseline_bin" --candidate-bin "$binary" \
            --families "${PERF_RUNTIME:?}" > "$result/allocations.json" || status=$?
        cat "$result/allocations.json"
        jq -r '.rows[] | "- " + (.args[0] // "unknown workload") + ": " + .decision' \
            "$result/allocations.json" >> "$GITHUB_STEP_SUMMARY"
        if [[ "$status" == 2 ]]; then
            echo 'Inconclusive: inspect the affected row. Do not retry until it passes. No parity claim.' >> "$GITHUB_STEP_SUMMARY"
        fi
        exit "$status"
        ;;
    *) echo 'usage: hosted_change_performance.sh release-base|check-release-baseline|select|prepare|measure' >&2; exit 2 ;;
esac
