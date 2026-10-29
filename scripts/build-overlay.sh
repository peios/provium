#!/bin/bash
# Build the declared guest archive with pekit.
set -euo pipefail
exec "$(dirname -- "${BASH_SOURCE[0]}")/build-archive.sh" overlay "$@"
