#!/usr/bin/env bash
# Real-QEMU smoke test for slice 3.
#
# Builds the initrd via build-initrd.sh, then runs the qemu_smoke
# example which boots a real VM, drives a few ops through
# `provium-agent`, and shuts down. Exits 0 on success.
#
# Requires:
#   * `/dev/kvm` accessible (membership in the `kvm` group).
#   * `qemu-system-x86_64` on PATH.
#   * A kernel image — defaults to the Peios bzImage at
#     `pkm/kernel/out/bzImage`; override via `KERNEL=...` env var.
#
# Usage:
#   scripts/smoke-real-qemu.sh
#   KERNEL=/path/to/bzImage scripts/smoke-real-qemu.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

KERNEL="${KERNEL:-$WORKSPACE/../pkm/kernel/out/bzImage}"
if [[ ! -f "$KERNEL" ]]; then
    echo "$0: kernel not found at $KERNEL" >&2
    echo "    set KERNEL=/path/to/bzImage to override" >&2
    exit 2
fi

if [[ ! -r /dev/kvm ]]; then
    echo "$0: /dev/kvm not readable; check kvm group membership" >&2
    exit 2
fi
if ! command -v qemu-system-x86_64 >/dev/null 2>&1; then
    echo "$0: qemu-system-x86_64 not on PATH" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
# Build the initrd
# ---------------------------------------------------------------------------

INITRD="$WORKSPACE/dist/initrd.cpio.gz"
echo "==> building initrd"
"$SCRIPT_DIR/build-initrd.sh" -o "$INITRD"

# ---------------------------------------------------------------------------
# Build + run the smoke example
# ---------------------------------------------------------------------------

echo
echo "==> cargo build --example qemu_smoke (release)"
cargo build \
    --manifest-path "$WORKSPACE/Cargo.toml" \
    -p provium-host \
    --release \
    --example qemu_smoke

EXAMPLE_BIN="$WORKSPACE/target/release/examples/qemu_smoke"

echo
echo "==> running qemu_smoke"
echo "    kernel: $KERNEL"
echo "    initrd: $INITRD"
echo

"$EXAMPLE_BIN" --kernel "$KERNEL" --initrd "$INITRD"
