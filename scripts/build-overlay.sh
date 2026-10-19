#!/bin/bash
# Build the declared guest archive through Bazel.
set -euo pipefail
exec "$(dirname -- "${BASH_SOURCE[0]}")/build-archive.sh" overlay "$@"
