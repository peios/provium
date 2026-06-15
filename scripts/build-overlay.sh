#!/usr/bin/env bash
# Build the agent-overlay cpio: a static `provium-agent` at
# `/sbin/provium-agent` plus the pseudo-FS mountpoint dirs the agent
# expects (`/proc`, `/sys`, `/dev`, `/tmp`).
#
# Concatenated with a user-supplied initrd at runtime by provium-host:
# the kernel unpacks both archives into the same rootfs, our binary
# lands at /sbin/provium-agent without colliding with the user's
# /init, and `rdinit=/sbin/provium-agent` on the kernel cmdline tells
# the kernel to invoke us as PID 1. The agent then forks: child
# becomes the listener, parent execs the user's /init.
#
# Mirrors scripts/build-initrd.sh's toolchain selection and
# byte-stable cpio incantation; only the layout differs (no /init).
#
# Usage:
#   scripts/build-overlay.sh              # release build, default output
#   scripts/build-overlay.sh --debug      # debug build
#   scripts/build-overlay.sh -o path.gz   # custom output path

set -euo pipefail

PROFILE=release
PROFILE_DIR=release
OUT=""

usage() {
    cat <<EOF
usage: $0 [--debug] [-o <path>]

  --debug         build debug artifacts (default: release)
  -o <path>       output path for the cpio.gz (default: dist/agent-overlay.cpio.gz)
  -h, --help      this help text
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --debug)
            PROFILE=dev
            PROFILE_DIR=debug
            shift
            ;;
        -o)
            OUT="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "$0: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET=x86_64-unknown-linux-musl

if [[ -z "$OUT" ]]; then
    OUT="$WORKSPACE/dist/agent-overlay.cpio.gz"
fi

if command -v rustup >/dev/null 2>&1; then
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
    STABLE_BIN="$RUSTUP_HOME/toolchains/stable-x86_64-unknown-linux-gnu/bin"
    if [[ -d "$STABLE_BIN" ]]; then
        export PATH="$STABLE_BIN:$PATH"
    fi
fi

for tool in cargo cpio gzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$0: $tool not in PATH" >&2
        exit 1
    fi
done

echo "==> cargo build --target $TARGET ($PROFILE)"
case "$PROFILE" in
    release)
        cargo build \
            --manifest-path "$WORKSPACE/Cargo.toml" \
            -p provium-agent \
            --target "$TARGET" \
            --release
        ;;
    dev)
        cargo build \
            --manifest-path "$WORKSPACE/Cargo.toml" \
            -p provium-agent \
            --target "$TARGET"
        ;;
esac

AGENT_BIN="$WORKSPACE/target/$TARGET/$PROFILE_DIR/provium-agent"
if [[ ! -x "$AGENT_BIN" ]]; then
    echo "$0: build did not produce $AGENT_BIN" >&2
    exit 1
fi

ROOT="$(mktemp -d -t provium-overlay-XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT

echo "==> assembling overlay tree at $ROOT"
mkdir -p "$ROOT"/{proc,sys,dev,tmp,sbin}
install -Dm755 "$AGENT_BIN" "$ROOT/sbin/provium-agent"

mkdir -p "$(dirname "$OUT")"
echo "==> packing $OUT"

find "$ROOT" -exec touch -d @0 {} +

( cd "$ROOT" && find . -mindepth 1 -print0 \
    | LC_ALL=C sort -z \
    | cpio --quiet --null --create --format=newc --reproducible --owner 0:0
) | gzip -n -9 > "$OUT"

OVERLAY_SIZE=$(stat -c %s "$OUT")
OVERLAY_HASH=$(sha256sum "$OUT" | awk '{print $1}')
AGENT_SIZE=$(stat -c %s "$AGENT_BIN")

echo
echo "  overlay:        $OUT"
echo "  size:           $(numfmt --to=iec-i --suffix=B "$OVERLAY_SIZE")"
echo "  sha256:         $OVERLAY_HASH"
echo "  agent binary:   $(numfmt --to=iec-i --suffix=B "$AGENT_SIZE")"
