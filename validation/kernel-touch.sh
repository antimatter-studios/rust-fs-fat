#!/usr/bin/env bash
# Have the Linux kernel's vfat driver write into images the Rust driver made.
#
#   kernel-touch.sh IMG...
#
# Runs as root INSIDE the Linux test VM (see scripts/vm-setup.sh). Each image
# is mounted read-write through a loop device on the guest's own disk, changed
# the way an ordinary user would (a directory, long and short and Unicode
# names, an append, a rename, a delete), unmounted, and copied back over IMG.
# The Rust test then reads the result with the driver and checks it with
# fsck.fat (inspect-image.sh).
set -euo pipefail

work="$(mktemp -d /var/tmp/fattouch.XXXXXX)"
mnt="$work/mnt"
mkdir "$mnt"
cleanup() {
    cd /
    if mountpoint -q "$mnt"; then umount "$mnt" || true; fi
    rm -rf "${work:?}"
}
trap cleanup EXIT

for img in "$@"; do
    local_img="$work/image"
    cp --sparse=always "$img" "$local_img"
    mount -o loop,iocharset=utf8,shortname=winnt "$local_img" "$mnt"
    (
        cd "$mnt"
        mkdir "Kernel directory"
        printf 'made by the kernel\n' >"Kernel made file with a long name.txt"
        printf 'short\n' >KERNEL.TXT
        printf 'lower\n' >kernel.low
        printf 'unicode\n' >"Ünïcödé kernel — résumé.txt"
        printf 'nested\n' >"Kernel directory/nested file with a long name.dat"
        printf 'appended by the kernel\n' >>multi.bin
        mv "Mixed Case.Txt" "Moved by the kernel with a long name.txt"
        rm -f docs/nested/deep.txt || true
        # A FAT12/16 root has a fixed number of slots and may already be full;
        # the kernel then says so, which is the behaviour being exercised.
        for i in $(seq 1 20); do printf '%s\n' "$i" >"kernel batch file with a long name $i.txt" || true; done
    )
    sync
    umount "$mnt"
    cp --sparse=always "$local_img" "$img"
    rm -f "$local_img"
done
