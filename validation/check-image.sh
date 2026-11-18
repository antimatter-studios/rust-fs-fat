#!/usr/bin/env bash
# Check images the Rust tests wrote, with tools that are not the driver.
#
#   check-image.sh IMG...
#
# Runs as root INSIDE the Linux test VM (see scripts/vm-setup.sh). For each
# IMG it writes IMG.check beside it:
#
#   fsck_exit<TAB>N          exit status of `fsck.fat -n IMG` (0 = clean)
#   fsck_output<TAB>LINE     one line per line of its output
#   mount<TAB>ok|failed      the kernel's vfat driver mounting it read-only
#   statfs<TAB>BSIZE TOTAL FREE TYPE   what the kernel reports (cluster size,
#                            total and free clusters, filesystem type)
#   entries<TAB>N            names in the mounted root directory
#   label<TAB>TEXT           `fatlabel IMG`
# and the tool versions once in tools.txt beside the first image.
#
# Nothing is committed but this recipe: images come from the tests, which
# rebuild them on every run.
set -uo pipefail

[ "$#" -gt 0 ] || { echo "usage: check-image.sh IMG..." >&2; exit 2; }
work="$(mktemp -d /var/tmp/fatcheck.XXXXXX)"
mnt="$work/mnt"
mkdir "$mnt"
cleanup() {
    cd /
    if mountpoint -q "$mnt"; then umount "$mnt" || true; fi
    rm -rf "${work:?}"
}
trap cleanup EXIT

{
    echo "kernel $(uname -r)"
    dpkg-query -W -f='${Package} ${Version}\n' dosfstools mtools
} >"$(dirname "$1")/tools.txt"

for img in "$@"; do
    out="$img.check"
    : >"$out"
    rc=0
    text="$(fsck.fat -n "$img" 2>&1)" || rc=$?
    printf 'fsck_exit\t%s\n' "$rc" >>"$out"
    printf '%s\n' "$text" | sed 's/^/fsck_output\t/' >>"$out"
    # An image cannot be fsck'd and mounted at once if the check holds it, so
    # the mount comes second. `loop` needs the image on the guest's own disk
    # for O_DIRECT-free access, so it is copied there.
    local_img="$work/image"
    cp --sparse=always "$img" "$local_img"
    if mount -o ro,loop,iocharset=utf8 "$local_img" "$mnt" 2>"$work/mount.err"; then
        printf 'mount\tok\n' >>"$out"
        printf 'statfs\t%s\n' "$(stat -f -c '%S %b %f %T' "$mnt")" >>"$out"
        printf 'entries\t%s\n' "$(ls -A "$mnt" | wc -l)" >>"$out"
        umount "$mnt"
    else
        printf 'mount\tfailed\n' >>"$out"
        sed 's/^/mount_error\t/' "$work/mount.err" >>"$out"
    fi
    printf 'label\t%s\n' "$(fatlabel "$img" 2>&1 | head -1)" >>"$out"
    rm -f "$local_img"
done
