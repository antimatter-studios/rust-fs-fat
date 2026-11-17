#!/usr/bin/env bash
# Generate the disposable FAT12/16/32 images and the independent oracle
# output the Rust tests (tests/independent_validation.rs) compare against.
#
#   generate.sh OUTDIR
#
# Runs as root INSIDE the Linux test VM (the tools and the kernel's vfat
# driver live there; see scripts/vm-setup.sh). Nothing here is committed
# except this recipe: images are rebuilt from it on every run, with
# deterministic content.
#
# Per base image NAME (fat12, fat16, fat32):
#   NAME.img            built by mkfs.fat, populated through the kernel vfat mount
#   NAME.fsck           `exit=N` then fsck.fat -n output
#   NAME.manifest       what the kernel mount holds: `d|f<TAB>size<TAB>path`,
#                       and `chain<TAB>path<TAB>clusters` from mtools mshowfat
#   NAME.tree/          the files, copied out through the kernel mount
# The kernel tree and an mtools (mcopy) tree must agree byte for byte, or
# this script fails: the oracles are checked against each other first.
#
# Damage recipes, each a copy of a base image (NAME-RECIPE):
#   deleted   three root files and one nested file removed through the kernel
#   truncated the FAT chain of docs/nested/deep.bin cut after its first
#             cluster, in every FAT copy (patch_fat.py)
#   nobootsig the 0x55 0xAA boot signature zeroed
#   zeroboot  sector 0 zeroed
#   badbpb    bytes-per-sector field set to 0
# Each has NAME-RECIPE.img, .fsck and .recipe (key<TAB>value lines). The
# deleted recipe also gets a .manifest and .tree (the kernel cannot read the others).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?usage: generate.sh OUTDIR}"
rm -rf "${out:?}"
mkdir -p "$out"
work="$(mktemp -d /var/tmp/fatval.XXXXXX)"
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
} >"$out/tools.txt"

# prng SEED BYTES: deterministic bytes, so a rebuilt image is identical.
prng() {
    local key
    key="$(printf '%s' "$1" | sha256sum | cut -c1-32)"
    set +o pipefail
    openssl enc -aes-128-ctr -K "$key" -iv 00000000000000000000000000000000 </dev/zero 2>/dev/null | head -c "$2"
    set -o pipefail
}

# put DEST SEED BYTES
put() { prng "$2" "$3" >"$1"; }

populate() { # CLUSTER_BYTES ROOT_FILLER_COUNT
    local cs="$1" filler="$2" i n=24
    cd "$mnt"
    put readme.txt readme 11
    put lower.txt lower 1500
    put UPPER.TXT upper 700
    put mixed.TXT mixedbase 33
    put UPEXT.txt upperext 44
    # shortname=winnt stores an all-lower-case 8.3 name as an upper-case
    # short entry plus the NT lower-case flags in byte 12, as Windows does.
    put ntlower.txt ntlower 21
    put ntbase.TXT ntbase 22
    put NTEXT.txt ntext 23
    : >empty.dat
    put "Mixed Case.Txt" mixedcase 2222
    put "Ünïcödé — résumé with a very long name.txt" unicode 3333
    put "a name that is much longer than eight dot three but not unicode.data" longname $((cs * 3 + 17))
    put exact.bin exact $((cs * 4))
    put onebyte.bin onebyte 1
    # Distinct sizes so a deleted record can be matched to its file.
    put gone-one.txt gone1 1001
    put gone-two.txt gone2 1002
    put gone-three.txt gone3 1003
    mkdir -p docs/nested/deeper
    put docs/guide.txt guide 5000
    put docs/nested/deep.bin deep $((cs * 6 + 5))
    put "docs/nested/Long Nested Name.txt" nestedlong 1234
    put docs/nested/deeper/leaf.bin leaf $((cs * 2))
    put docs/nested/gone-nested.txt gonenested 1004
    # Fragmentation: two files grown one cluster at a time, alternately, so
    # each cluster of one sits between clusters of the other.
    put "$work/fa.src" fraga $((cs * n))
    put "$work/fb.src" fragb $((cs * n))
    : >frag-a.bin
    : >frag-b.bin
    for ((i = 0; i < n; i++)); do
        dd if="$work/fa.src" of=frag-a.bin bs="$cs" skip="$i" seek="$i" count=1 conv=notrunc status=none
        dd if="$work/fb.src" of=frag-b.bin bs="$cs" skip="$i" seek="$i" count=1 conv=notrunc status=none
    done
    # Enough long-named root entries to spill past one cluster, which makes
    # a FAT32 root a chain and fills most of a FAT12/16 fixed root.
    for ((i = 0; i < filler; i++)); do
        printf 'filler %04d\n' "$i" >"$(printf 'filler file with a long name %04d.txt' "$i")"
    done
    cd "$work"
    sync
}

# Oracle output for the image at $1, named $2 in $out. Mounts it read-only
# through the kernel, copies the tree out, copies it out again with mtools,
# and requires the two to agree.
oracle() {
    local img="$1" name="$2" tree="$out/$2.tree" mt="$work/mtools-tree"
    mount -o ro,loop,iocharset=utf8,shortname=winnt "$img" "$mnt"
    mkdir "$tree"
    cp -r --no-preserve=mode,ownership,timestamps "$mnt"/. "$tree"/
    umount "$mnt"
    rm -rf "${mt:?}"
    mkdir "$mt"
    mcopy -i "$img" -s -n -m ::/ "$mt"/
    if ! diff -r "$tree" "$mt" >"$work/diff.txt"; then
        echo "generate: kernel vfat and mtools disagree on $name:" >&2
        head -20 "$work/diff.txt" >&2
        exit 1
    fi
    {
        (cd "$tree" && find . -mindepth 1 \( -type d -printf 'd\t0\t%P\n' -o -type f -printf 'f\t%s\t%P\n' \)) | LC_ALL=C sort
        (cd "$tree" && find . -type f -print0 | LC_ALL=C sort -z) | while IFS= read -r -d '' p; do
            p="${p#./}"
            printf 'chain\t%s\t%s\n' "$p" "$(chain_of "$img" "$p" | paste -sd, -)"
        done
    } >"$out/$name.manifest"
}

fsck_record() { # IMG NAME
    local rc=0 text
    text="$(fsck.fat -n "$1" 2>&1)" || rc=$?
    printf 'exit=%s\n%s\n' "$rc" "$text" >"$out/$2.fsck"
}

publish() { cp --sparse=always "$1" "$out/$2.img"; }

# chain_of IMG PATH: the clusters mtools' mshowfat reports, one per line.
chain_of() {
    mshowfat -i "$1" "::/$2" | grep -o '<[^>]*>' | tr -d '<>' | tr ',' '\n' |
        awk -F- '{ if (NF == 2) for (i = $1; i <= $2; i++) print i; else print $1 }'
}

build() { # NAME KIND MKFS_FLAGS SIZE_KIB CLUSTER_BYTES FILLER
    local name="$1" kind="$2" flags="$3" size="$4" cs="$5" filler="$6" img="$work/$1.img"
    # shellcheck disable=SC2086
    mkfs.fat -F "$kind" $flags -n "VAL$kind" -i "0000$kind" -C "$img" "$size" >/dev/null
    mount -o loop,iocharset=utf8,shortname=winnt "$img" "$mnt"
    populate "$cs" "$filler"
    umount "$mnt"
    publish "$img" "$name"
    fsck_record "$img" "$name"
    oracle "$img" "$name"
}

damage() { # NAME KIND RECIPE
    local name="$1" kind="$2" recipe="$3" base="$work/$1.img" tag="$1-$3" p first
    local img="$work/$tag.img"
    cp --sparse=always "$base" "$img"
    : >"$out/$tag.recipe"
    case "$recipe" in
    deleted)
        mount -o loop,iocharset=utf8,shortname=winnt "$img" "$mnt"
        for p in gone-one.txt gone-two.txt gone-three.txt docs/nested/gone-nested.txt; do
            printf 'deleted\t%s\t%s\n' "$(stat -c %s "${mnt:?}/$p")" "$p" >>"$out/$tag.recipe"
            rm "${mnt:?}/${p:?}"
        done
        umount "$mnt"
        ;;
    truncated)
        first="$(chain_of "$img" docs/nested/deep.bin | sed -n 1p)"
        printf 'truncated\t%s\t%s\n' docs/nested/deep.bin "$(chain_of "$img" docs/nested/deep.bin | wc -l)" >>"$out/$tag.recipe"
        python3 "$here/patch_fat.py" "$img" "$kind" "$first" 0xFFFFFFF
        ;;
    nobootsig)
        printf '\0\0' | dd of="$img" bs=1 seek=510 conv=notrunc status=none
        printf 'boot\tsignature zeroed\n' >>"$out/$tag.recipe"
        ;;
    zeroboot)
        dd if=/dev/zero of="$img" bs=512 count=1 conv=notrunc status=none
        printf 'boot\tsector 0 zeroed\n' >>"$out/$tag.recipe"
        ;;
    badbpb)
        printf '\0\0' | dd of="$img" bs=1 seek=11 conv=notrunc status=none
        printf 'boot\tbytes per sector set to 0\n' >>"$out/$tag.recipe"
        ;;
    esac
    publish "$img" "$tag"
    fsck_record "$img" "$tag"
    case "$recipe" in
    deleted) oracle "$img" "$tag" ;;
    esac
}

build fat12 12 "-s 1 -S 512" 1440 512 28
build fat16 16 "-s 4 -S 512" 16384 2048 60
build fat32 32 "-s 1 -S 512" 40960 512 70

for spec in "fat12 12" "fat16 16" "fat32 32"; do
    # shellcheck disable=SC2086
    set -- $spec
    for recipe in deleted truncated nobootsig zeroboot badbpb; do
        damage "$1" "$2" "$recipe"
    done
done
echo "generate: $(find "$out" -name '*.img' | wc -l) images in $out"
