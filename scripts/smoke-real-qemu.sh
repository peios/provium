#!/bin/bash
# Explicit guest qualification, separate from ordinary host checks.
set -euo pipefail
: "${KERNEL:?Set KERNEL to the kernel image to qualify}"
[[ -r "$KERNEL" && -r /dev/kvm ]] || {
    echo "The kernel image and /dev/kvm must be readable." >&2
    exit 2
}
command -v qemu-system-x86_64 >/dev/null
kernel=$(realpath -- "$KERNEL")
workspace=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$workspace"
# pekit prints each output's path, indented, under the target it built.
initrd=$(pekit build agent_initrd | sed -n 's/^  //p')
runner=$(pekit build qemu_smoke | sed -n 's/^  //p')
exec "$workspace/$runner" --kernel "$kernel" --initrd "$workspace/$initrd"
