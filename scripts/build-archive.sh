#!/bin/bash
# Compatibility entry point; Bazel owns compilation and archive construction.
set -euo pipefail
kind=${1:?archive target required}
shift
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
profile=opt
output=""
usage() {
    echo "usage: build-$kind.sh [--debug] [-o <path>]"
}
while (($#)); do
    case "$1" in
    --debug)
        profile=dbg
        shift
        ;;
    -o)
        if (($# < 2)); then
            usage >&2
            exit 2
        fi
        output=$2
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
    esac
done
case "$kind" in
overlay)
    target=agent-overlay
    default=agent-overlay.cpio.gz
    ;;
initrd)
    target=agent-initrd
    default=initrd.cpio.gz
    ;;
*)
    usage >&2
    exit 2
    ;;
esac
output=${output:-$workspace/dist/$default}
case "$output" in /*) ;; *) output="$PWD/$output" ;; esac
cd "$workspace"
./bazel build -c "$profile" "//:$target"
artifact=$(./bazel cquery -c "$profile" "//:$target" --output=files)
mkdir -p -- "$(dirname -- "$output")"
cp -- "$artifact" "$output"
printf '%s\n' "$output"
