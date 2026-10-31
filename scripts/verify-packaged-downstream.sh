#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "${repo_root}"

for required_tool in git ssh-keygen python3; do
    if ! command -v "${required_tool}" >/dev/null; then
        echo "required qualification tool unavailable: ${required_tool}" >&2
        exit 1
    fi
done
git_version=$(git --version)
if [[ ! ${git_version} =~ ^git\ version\ ([0-9]+)\.([0-9]+) ]] ||
    (( BASH_REMATCH[1] < 2 || (BASH_REMATCH[1] == 2 && BASH_REMATCH[2] < 34) )); then
    echo 'registry SSH signing requires Git 2.34 or newer' >&2
    exit 1
fi

toolchain_bin=$(dirname "${RUSTC:-$(rustup which --toolchain 1.99.0 cargo)}")
export PATH="${toolchain_bin}:${PATH}"
registry_ssh_keygen=$(command -v ssh-keygen)

package_version='0.1.0'
package_rust_version='1.99.0'
expected_rustc="rustc ${package_rust_version}"
expected_cargo="cargo ${package_rust_version}"
if [[ $(rustc --version) != "${expected_rustc}"* ]]; then
    echo "expected ${expected_rustc}" >&2
    exit 1
fi
if [[ $(cargo --version) != "${expected_cargo}"* ]]; then
    echo "expected ${expected_cargo}" >&2
    exit 1
fi

package_names=(
    rca-event-listener
    rca-event-listener-strategy
    rca-async-channel
    rca-macros
    rca-core
    rca-test
    rca-tokio
    rca-persistence
    rust-composable-architecture
)
fixture_names=(
    downstream-packaged
    downstream-composition
    downstream-minimal-core
    downstream-minimal-facade
    downstream-ecosystem
    downstream-persistence
    downstream-file
    packaged-examples/counter
    packaged-examples/search
    packaged-examples/timer
    packaged-examples/composition-search
)
fixture_manifests=()
for fixture_name in "${fixture_names[@]}"; do
    fixture_manifests+=("${repo_root}/tests/${fixture_name}/Cargo.toml")
done

registry_helper="${repo_root}/scripts/package_registry.py"
python3 "${registry_helper}" validate-fixtures \
    --version "${package_version}" \
    "${fixture_manifests[@]}"
fixture_names+=(readme-quickstart readme-keyed)

qualification_parent=${TMPDIR:-/tmp}
qualification_parent=${qualification_parent%/}
qualification_root=$(mktemp -d "${qualification_parent}/rca-package-qualification.XXXXXX")
qualification_completed=false
cleanup() {
    local status=$?
    local cleanup_status=0
    case "${qualification_root}" in
        "${qualification_parent}"/rca-package-qualification.*)
            rm -rf -- "${qualification_root}" || cleanup_status=$?
            ;;
        *)
            echo "refusing to remove unexpected path: ${qualification_root}" >&2
            cleanup_status=1
            ;;
    esac
    if (( cleanup_status != 0 )); then
        echo "failed to remove qualification directory: ${qualification_root}" >&2
    fi
    if (( status != 0 )); then
        return "${status}"
    fi
    # Bash 3 can report zero to EXIT after a fatal nounset expansion.
    if [[ ${qualification_completed} != true ]]; then
        return 1
    fi
    return "${cleanup_status}"
}
trap cleanup EXIT

# The disposable signer never enters the index/downloads or personal Git config.
# Exercise SSH signing/verification before any expensive packaging operation.
registry_signing_dir="${qualification_root}/signing"
mkdir -m 700 "${registry_signing_dir}"
"${registry_ssh_keygen}" -q -t ed25519 -N '' \
    -C 'qualification@invalid.example' -f "${registry_signing_dir}/key"
printf 'qualification@invalid.example %s\n' "$(<"${registry_signing_dir}/key.pub")" \
    > "${registry_signing_dir}/allowed-signers"
printf 'registry signing preflight\n' | "${registry_ssh_keygen}" -Y sign \
    -n git -f "${registry_signing_dir}/key" > "${registry_signing_dir}/preflight.sig"
printf 'registry signing preflight\n' | "${registry_ssh_keygen}" -Y verify \
    -n git -I 'qualification@invalid.example' \
    -f "${registry_signing_dir}/allowed-signers" -s "${registry_signing_dir}/preflight.sig"
rm -- "${registry_signing_dir}/preflight.sig"
registry_signing_config=(
    -c gpg.format=ssh
    -c "gpg.ssh.program=${registry_ssh_keygen}"
    -c "user.signingkey=${registry_signing_dir}/key"
    -c "gpg.ssh.allowedSignersFile=${registry_signing_dir}/allowed-signers"
)
registry_git() {
    env -u GIT_CONFIG -u GIT_CONFIG_PARAMETERS \
        GIT_CONFIG_COUNT=0 GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
        git -C "${registry_index}" "${registry_signing_config[@]}" "$@"
}

registry_index="${qualification_root}/index"
registry_downloads="${qualification_root}/downloads"
consumers_root="${qualification_root}/consumers"
qualification_cargo_home="${qualification_root}/cargo-home"
qualification_target="${CARGO_TARGET_DIR:-${qualification_root}/target}"
package_target="${CARGO_TARGET_DIR:-${repo_root}/target}"
core_verification_root="${qualification_root}/core-verification"
mkdir -p \
    "${registry_index}" \
    "${registry_downloads}" \
    "${qualification_cargo_home}" \
    "${core_verification_root}" \
    "${consumers_root}"
for fixture_name in "${fixture_names[@]}"; do
    if [[ ${fixture_name} == readme-* ]]; then
        readme_options=()
        if [[ ${fixture_name} == readme-keyed ]]; then
            readme_options+=(--keyed)
        fi
        # Bash 3 treats an empty array as unset under nounset.
        python3 "${repo_root}/scripts/readme_quickstart.py" \
            "${repo_root}/README.md" "${consumers_root}/${fixture_name}" ${readme_options[@]+"${readme_options[@]}"}
        continue
    fi
    mkdir -p "$(dirname "${consumers_root}/${fixture_name}")"
    cp -R "${repo_root}/tests/${fixture_name}" "${consumers_root}/${fixture_name}"
    if [[ ${fixture_name} == packaged-examples/* ]]; then
        example_root="${repo_root}/examples/${fixture_name##*/}"
        cp -R "${example_root}/src" "${consumers_root}/${fixture_name}/src"
        if [[ -d ${example_root}/tests ]]; then
            cp -R "${example_root}/tests" "${consumers_root}/${fixture_name}/tests"
        fi
    fi
done

transport_patch=(
    --config 'patch.crates-io.rca-event-listener.path="dependencies/rca-event-listener"'
    --config 'patch.crates-io.rca-event-listener-strategy.path="dependencies/rca-event-listener-strategy"'
    --config 'patch.crates-io.rca-async-channel.path="dependencies/rca-async-channel"'
)
for transport_package in rca-event-listener rca-event-listener-strategy rca-async-channel; do
    cargo package \
        --locked \
        --allow-dirty \
        --no-verify \
        --package "${transport_package}" \
        "${transport_patch[@]}"
done

cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    --package rca-macros \
    "${transport_patch[@]}" \
    --config 'patch.crates-io.rca-core.path="rca-core"' \
    --config 'patch.crates-io.rca-test.path="rca-test"'
cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    --package rca-core \
    "${transport_patch[@]}" \
    --config 'patch.crates-io.rca-macros.path="rca-macros"'
cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    --package rca-test \
    "${transport_patch[@]}" \
    --config 'patch.crates-io.rca-core.path="rca-core"' \
    --config 'patch.crates-io.rca-macros.path="rca-macros"'
cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    --package rca-tokio \
    "${transport_patch[@]}" \
    --config 'patch.crates-io.rca-core.path="rca-core"' \
    --config 'patch.crates-io.rca-test.path="rca-test"'
cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    --all-features \
    --package rca-persistence \
    "${transport_patch[@]}" \
    --config 'patch.crates-io.rca-core.path="rca-core"' \
    --config 'patch.crates-io.rca-test.path="rca-test"'
cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    --all-features \
    --package rust-composable-architecture \
    "${transport_patch[@]}" \
    --config 'patch.crates-io.rca-core.path="rca-core"' \
    --config 'patch.crates-io.rca-macros.path="rca-macros"' \
    --config 'patch.crates-io.rca-test.path="rca-test"' \
    --config 'patch.crates-io.rca-tokio.path="rca-tokio"'

package_archives=()
for package_name in "${package_names[@]}"; do
    package_archives+=(
        "${package_target}/package/${package_name}-${package_version}.crate"
    )
done

python3 "${registry_helper}" stage-archives \
    --index "${registry_index}" \
    --downloads "${registry_downloads}" \
    --version "${package_version}" \
    --rust-version "${package_rust_version}" \
    "${package_archives[@]}"

registry_git init --quiet
registry_git config user.name 'RCA Qualification'
registry_git config user.email 'qualification@invalid.example'
registry_git add .
registry_git commit --quiet --gpg-sign \
    -m 'test: staged package archives' \
    -m '- Recorded package checksums and dependency metadata for isolated registry verification.'
registry_git verify-commit HEAD
rm -- "${registry_signing_dir}/key"

python3 "${registry_helper}" write-config \
    --index "${registry_index}" \
    "${qualification_cargo_home}/config.toml"
export CARGO_HOME="${qualification_cargo_home}"
export CARGO_TARGET_DIR="${qualification_target}"
core_verification_archive="${registry_downloads}/rca-core-${package_version}.crate"
tar -xzf "${core_verification_archive}" -C "${core_verification_root}"
core_verification_manifest="${core_verification_root}/rca-core-${package_version}/Cargo.toml"
core_verification_metadata="${qualification_root}/core-metadata.json"
registry_patch=(
    --config 'patch.crates-io.rca-macros.registry="rca-local"'
    --config "patch.crates-io.rca-macros.version=\"=${package_version}\""
)
for transport_package in rca-event-listener rca-event-listener-strategy rca-async-channel; do
    registry_patch+=(
        --config "patch.crates-io.${transport_package}.registry=\"rca-local\""
        --config "patch.crates-io.${transport_package}.version=\"=${package_version}\""
    )
done
cargo metadata \
    --format-version 1 \
    --manifest-path "${core_verification_manifest}" \
    "${registry_patch[@]}" \
    > "${core_verification_metadata}"
for dependency_package in rca-macros rca-event-listener rca-event-listener-strategy rca-async-channel; do
    python3 "${registry_helper}" validate-registry-source \
        --metadata "${core_verification_metadata}" \
        --package "${dependency_package}" \
        --registry-index "file://${registry_index}"
done
export CARGO_NET_OFFLINE=true
cargo check \
    --manifest-path "${core_verification_manifest}" \
    --locked \
    --offline \
    --all-targets \
    --all-features \
    "${registry_patch[@]}"

unset CARGO_NET_OFFLINE
select_consumer() {
    consumer_manifest="${consumers_root}/${fixture_name}/Cargo.toml"
    consumer_registry_patch=()
    if [[ ${fixture_name} == readme-* ]]; then
        # Preserve the literal manifest; only redirect the unpublished package's source.
        consumer_registry_patch=(
            --config 'patch.crates-io.rust-composable-architecture.registry="rca-local"'
            --config "patch.crates-io.rust-composable-architecture.version=\"=${package_version}\""
        )
    fi
}
for fixture_name in "${fixture_names[@]}"; do
    select_consumer
    cargo generate-lockfile --manifest-path "${consumer_manifest}" ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"}
    cargo fetch --manifest-path "${consumer_manifest}" --locked ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"}
done

export CARGO_NET_OFFLINE=true
for fixture_name in "${fixture_names[@]}"; do
    echo "verifying registry consumer: ${fixture_name}"
    select_consumer
    consumer_metadata="${qualification_root}/consumer-metadata.json"
    cargo metadata --format-version 1 --manifest-path "${consumer_manifest}" --locked --offline ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"} > "${consumer_metadata}"
    python3 "${registry_helper}" validate-consumer-source \
        --metadata "${consumer_metadata}" --version "${package_version}" \
        --registry-index "file://${registry_index}"
    if [[ ${fixture_name} == downstream-* && ${fixture_name} != downstream-packaged && ${fixture_name} != downstream-composition ]]; then
        cargo tree --manifest-path "${consumer_manifest}" --locked --offline --edges normal
    fi
    if [[ ${fixture_name} == downstream-file ]]; then
        for optional_package in rca-persistence tokio tempfile; do
            cargo tree --manifest-path "${consumer_manifest}" --locked --offline --edges normal --invert "${optional_package}"
        done
    fi
    cargo check --manifest-path "${consumer_manifest}" --locked --offline ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"}
    cargo clippy --manifest-path "${consumer_manifest}" --all-targets --locked --offline \
        ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"} -- -D warnings
    cargo test --manifest-path "${consumer_manifest}" --locked --offline ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"}
    cargo run --manifest-path "${consumer_manifest}" --locked --offline ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"}
    if [[ ${fixture_name} == downstream-ecosystem ]]; then
        cargo run --release --manifest-path "${consumer_manifest}" --locked --offline \
            ${consumer_registry_patch[@]+"${consumer_registry_patch[@]}"}
    fi
done

echo "packaged downstream qualification passed: ${#package_names[@]} archives, ${#fixture_names[@]} consumers"
qualification_completed=true
