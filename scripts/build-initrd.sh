#!/usr/bin/env bash
# Build the slice-3 test initrd: a static `provium-agent` as `/init`
# plus the empty mount-point directories `provium-agent`'s init logic
# expects (`/proc`, `/sys`, `/dev`, `/tmp`).
#
# The agent is responsible for the rest of init's duties (mounting
# pseudo-FSes etc.) — see `provium-agent/src/init.rs`.
#
# Usage:
#   scripts/build-initrd.sh              # release build, default output
#   scripts/build-initrd.sh --debug      # debug build
#   scripts/build-initrd.sh -o path.gz   # custom output path
#
# The script auto-detects the Rust toolchain. If `rustup` is on
# `PATH` and has a stable toolchain with the
# `x86_64-unknown-linux-musl` target installed, it is used; otherwise
# the script falls back to whatever `cargo` is in `PATH` (which must
# already support the musl target).

set -euo pipefail

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

PROFILE=release
PROFILE_DIR=release
OUT=""

usage() {
    cat <<EOF
usage: $0 [--debug] [-o <path>]

  --debug         build debug artifacts (default: release)
  -o <path>       output path for the cpio.gz (default: dist/initrd.cpio.gz)
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

# ---------------------------------------------------------------------------
# Locate workspace + tools
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET=x86_64-unknown-linux-musl

if [[ -z "$OUT" ]]; then
    OUT="$WORKSPACE/dist/initrd.cpio.gz"
fi

# Pick the cargo that has the musl target installed. Prefer rustup's
# stable toolchain when available — Nix-managed cargo on this system
# does not see rustup-installed targets.
if command -v rustup >/dev/null 2>&1; then
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
    STABLE_BIN="$RUSTUP_HOME/toolchains/stable-x86_64-unknown-linux-gnu/bin"
    if [[ -d "$STABLE_BIN" ]]; then
        export PATH="$STABLE_BIN:$PATH"
    fi
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "$0: cargo not in PATH" >&2
    exit 1
fi
if ! command -v cpio >/dev/null 2>&1; then
    echo "$0: cpio not in PATH (apt: cpio; nix: cpio)" >&2
    exit 1
fi
if ! command -v gzip >/dev/null 2>&1; then
    echo "$0: gzip not in PATH" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

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

# ---------------------------------------------------------------------------
# Assemble root tree
# ---------------------------------------------------------------------------

ROOT="$(mktemp -d -t provium-initrd-XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT

echo "==> assembling root tree at $ROOT"
mkdir -p "$ROOT"/{proc,sys,dev,tmp}
install -Dm755 "$AGENT_BIN" "$ROOT/init"

# ---------------------------------------------------------------------------
# cpio + gzip
# ---------------------------------------------------------------------------

mkdir -p "$(dirname "$OUT")"
echo "==> packing $OUT"

# cpio newc format is what the Linux kernel expects for initrds.
# Build a byte-stable artifact: zero every file's mtime, fixed sort
# order, --reproducible, --owner 0:0, gzip -n.
#
# The combination matters — --reproducible alone zeroes device/inode
# numbers but not mtimes; touching is what eliminates run-to-run
# drift in cpio output. Needed for the fixture cache key in slice 4.
find "$ROOT" -exec touch -d @0 {} +

( cd "$ROOT" && find . -mindepth 1 -print0 \
    | LC_ALL=C sort -z \
    | cpio --quiet --null --create --format=newc --reproducible --owner 0:0
) | gzip -n -9 > "$OUT"

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

INITRD_SIZE=$(stat -c %s "$OUT")
INITRD_HASH=$(sha256sum "$OUT" | awk '{print $1}')
AGENT_SIZE=$(stat -c %s "$AGENT_BIN")

echo
echo "  initrd:         $OUT"
echo "  size:           $(numfmt --to=iec-i --suffix=B "$INITRD_SIZE")"
echo "  sha256:         $INITRD_HASH"
echo "  agent binary:   $(numfmt --to=iec-i --suffix=B "$AGENT_SIZE")"
