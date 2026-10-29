#!/usr/bin/env bash
# Supply the CI passphrase through a private file descriptor, not command arguments.
set +x
set -euo pipefail

: "${DOCS_SIGNING_PASSPHRASE:?Documentation signing requires its passphrase}"
exec gpg --batch --pinentry-mode loopback --passphrase-fd 3 "$@" 3<<<"${DOCS_SIGNING_PASSPHRASE}"
