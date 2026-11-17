#!/usr/bin/env bash
# Rebuild the two small committed volumes the command-line tests use,
# tests/cli/sample-fat12.img.gz and tests/cli/sample-fat32.img.gz, with the
# independent tools: mkfs.fat formats them and mtools (mcopy, mmd, mdel)
# fills them, so neither volume was made by this driver. mshowfat records the
# cluster chains, which tests/cli.rs and the shell tests compare with `chain`.
#
#   make-cli-samples.sh OUTDIR     (OUTDIR is tests/cli)
#
# Runs where mkfs.fat and mtools are, typically INSIDE the Linux test VM:
#
#   vm.sh exec 'bash /repo/validation/make-cli-samples.sh /repo/tests/cli'
#
# Unprivileged: mtools works on the image file, nothing is mounted. The
# file CONTENT is deterministic; the timestamps mtools writes are not, so a
# rebuild gives different volumes, and samples.sha256 is rewritten with them.
# OUTDIR gets
#   sample-fat12.img.gz  sample-fat32.img.gz   gzip -9n of the volumes
#   samples.sha256       the sha256 of each UNCOMPRESSED volume, which the
#                        tests hold the decompressed bytes to
#   sample-fat12.manifest  sample-fat32.manifest
#                        `f|d<TAB>size<TAB>path`, `chain<TAB>path<TAB>clusters`
#                        and `deleted<TAB>path<TAB>size`
set -euo pipefail

out="${1:?usage: make-cli-samples.sh OUTDIR}"
mkdir -p "$out"
work="$(mktemp -d /var/tmp/fatsample.XXXXXX)"
trap 'rm -rf "${work:?}"' EXIT

lines() { # PREFIX COUNT BYTES: COUNT numbered lines, cut to BYTES
    local i
    set +o pipefail # head -c closes the pipe, which is not a failure here
    for ((i = 1; i <= $2; i++)); do printf '%s line %04d\n' "$1" "$i"; done | head -c "$3"
    set -o pipefail
}

chain_of() { # IMG PATH
    mshowfat -i "$1" "::/$2" | grep -o '<[^>]*>' | tr -d '<>' | tr ',' '\n' |
        awk -F- '{ if (NF == 2) for (i = $1; i <= $2; i++) print i; else print $1 }'
}

build() { # NAME FAT_BITS SIZE_KIB SERIAL
    local name="$1" img="$work/$1.img" src="$work/$1.src" manifest="$out/sample-$1.manifest"
    mkdir -p "$src/docs/deep"
    printf 'sample fat volume\n' >"$src/README.TXT"
    lines guide 200 1300 >"$src/docs/Guide With Spaces.txt"
    lines leaf 400 2000 >"$src/docs/deep/leaf.bin"
    lines gone 100 700 >"$src/gone.txt"
    : >"$src/empty.dat"

    mkfs.fat -F "$2" -s 1 -S 512 -n "SAMPLE$2" -i "$4" -C "$img" "$3" >/dev/null
    mmd -i "$img" ::/docs ::/docs/deep
    mcopy -i "$img" "$src/README.TXT" "$src/gone.txt" "$src/empty.dat" ::/
    mcopy -i "$img" "$src/docs/Guide With Spaces.txt" ::/docs/
    mcopy -i "$img" "$src/docs/deep/leaf.bin" ::/docs/deep/
    {
        printf 'd\t0\tdocs\nd\t0\tdocs/deep\n'
        for p in README.TXT empty.dat "docs/Guide With Spaces.txt" docs/deep/leaf.bin; do
            printf 'f\t%s\t%s\n' "$(wc -c <"$src/$p" | tr -d ' ')" "$p"
        done
        for p in README.TXT "docs/Guide With Spaces.txt" docs/deep/leaf.bin; do
            printf 'chain\t%s\t%s\n' "$p" "$(chain_of "$img" "$p" | paste -sd, -)"
        done
        printf 'deleted\tgone.txt\t%s\tchain\t%s\n' "$(wc -c <"$src/gone.txt" | tr -d ' ')" "$(chain_of "$img" gone.txt | paste -sd, -)"
    } >"$manifest"
    mdel -i "$img" ::/gone.txt
    fsck.fat -n "$img" >/dev/null
    gzip -9n -c "$img" >"$out/sample-$1.img.gz"
    (cd "$work" && sha256sum "$1.img") | awk '{ print $1 "  sample-'"$1"'.img" }'
}

{
    build fat12 12 1440 12121212
    build fat32 32 34000 32323232
} >"$out/samples.sha256"
ls -l "$out"
