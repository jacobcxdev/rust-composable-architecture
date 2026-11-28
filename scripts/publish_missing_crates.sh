#!/usr/bin/env bash
# Upload missing crates only if their archives match the files approved for release.
set -euo pipefail
cd "$(dirname "$0")/.."
version=${1:?usage: bash scripts/publish_missing_crates.sh VERSION}
[[ $# == 1 && ${GITHUB_REF_NAME:?} == "v${version}" ]]
upload_token=${CARGO_REGISTRY_TOKEN:-}
unset CARGO_REGISTRY_TOKEN

scratch=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/rca-crate-upload.XXXXXXXX")
trap 'rm -r -- "${scratch}"' EXIT
registry=(python3 scripts/check_registry_archives.py --version "${version}" --archives target/package)
tag_check() {
    local verified
    verified=$(python3 scripts/check_release_tag.py --tag "${GITHUB_REF_NAME}" --sha "${GITHUB_SHA:?}")
    [[ ${verified} == "${version}" ]]
}

for ((attempt = 0; ; attempt++)); do
    "${registry[@]}" > "${scratch}/missing"
    if [[ ! -s ${scratch}/missing ]]; then
        tag_check
        break
    fi
    if [[ -z ${upload_token} ]]; then
        echo 'Missing CARGO_REGISTRY_TOKEN. No upload was attempted.' >&2
        exit 1
    fi

    options=(--locked --registry crates-io --no-verify --target-dir "${scratch}/target")
    while IFS= read -r name; do
        options+=(--package "${name}")
    done < "${scratch}/missing"
    tag_check
    # Cargo gives its provider the actual tarball checksum immediately before
    # each upload. Do not let Cargo use an unchecked token from its environment.
    if PATH="$PWD/scripts:$PATH" RCA_RELEASE_MISSING="${scratch}/missing" RCA_RELEASE_TOKEN="${upload_token}" \
        cargo publish --config 'registry.credential-provider="cargo_release_credential.py"' \
        "${options[@]}" > "${scratch}/publish.log" 2>&1; then
        cat "${scratch}/publish.log"
        break
    fi
    cat "${scratch}/publish.log" >&2
    # A lost reply is not permission to repeat an upload. Only retry an explicit
    # crates.io rejection during first publication, with at most eight waits.
    if [[ ${version} != 0.1.0 || ${attempt} -ge 8 ]] ||
        ! grep -Eq '^error: failed to publish [a-z0-9_-]+ v0\.1\.0 to registry at https://crates\.io/?$' "${scratch}/publish.log" ||
        ! grep -Eq '^[[:space:]]*the remote server responded with an error \(status 429 Too Many Requests\)' "${scratch}/publish.log"; then
        exit 1
    fi
    sleep 600
done

# Cargo may finish before the public sparse index serves the new checksums.
if "${registry[@]}" --require-complete 2> "${scratch}/index-error"; then
    exit 0
fi
cat "${scratch}/index-error" >&2
grep -q '^missing from crates.io: ' "${scratch}/index-error" || exit 1
sleep 15
"${registry[@]}" --require-complete
