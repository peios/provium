#!/bin/bash
# Compatibility entry point; pekit owns compilation and archive construction.
set -euo pipefail
kind=${1:?archive target required}
shift
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output=""
usage() {
    echo "usage: build-$kind.sh [-o <path>]"
}
while (($#)); do
    case "$1" in
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
    target=agent_overlay
    default=agent-overlay.cpio.gz
    ;;
initrd)
    target=agent_initrd
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
# pekit prints each output's path, indented, under the target it built.
artifact=$(pekit build "$target" | sed -n 's/^  //p')
mkdir -p -- "$(dirname -- "$output")"
cp -- "$artifact" "$output"
printf '%s\n' "$output"
