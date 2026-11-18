# A boot sector whose 0x55 0xAA signature is zero, with the rest of the BPB
# intact: info reports it, every read verb works with one warning line on
# stderr and exit 0, a FAT32 backup is signed, and none of it writes.
source "$(dirname "$0")/lib.sh"

for kind in fat12 fat32; do
    img="$(sample_image "$kind")"
    reference="$SANDBOX/signed-$kind.img"
    cp "$img" "$reference"
    printf '\0\0' | dd of="$img" bs=1 seek=510 conv=notrunc status=none
    before="$(sha256_of "$img")"

    fs.fat "$img" info >"$SANDBOX/info.json" 2>"$SANDBOX/info.err"
    check "$kind: info exits 0 ($(cat "$SANDBOX/info.err"))" test $? -eq 0
    jq_check "$kind: info calls the volume valid, flags the signature and warns" \
        '.valid == true and .problem == null and .boot_sector.signature_valid == false and (.warnings | length) == 1 and (.warnings[0] | contains("0x55 0xAA"))' "$SANDBOX/info.json"

    fs.fat "$img" ls -R / >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"
    check "$kind: ls exits 0" test $? -eq 0
    fs.fat "$reference" ls -R / >"$SANDBOX/ref.json" 2>/dev/null
    check "$kind: ls lists what the signed volume lists" cmp -s "$SANDBOX/ls.json" "$SANDBOX/ref.json"
    check "$kind: ls prints exactly one warning line on stderr" test "$(grep -c '^warning:' "$SANDBOX/ls.err")" -eq 1
    check "$kind: the warning names the signature" grep -q '0x55 0xAA' "$SANDBOX/ls.err"

    fs.fat "$img" cat /README.TXT >"$SANDBOX/readme" 2>"$SANDBOX/cat.err"
    check "$kind: cat exits 0" test $? -eq 0
    check "$kind: cat reads the file" test "$(cat "$SANDBOX/readme")" = "sample fat volume"
    check "$kind: cat warns once" test "$(grep -c '^warning:' "$SANDBOX/cat.err")" -eq 1

    if [ "$kind" = fat32 ]; then
        jq_check "fat32: info finds the signed backup" \
            '.backup.accepted == true and .backup.signature_valid == true' "$SANDBOX/info.json"
        fs.fat "$img" --boot-sector 6 cat /README.TXT >"$SANDBOX/backup" 2>"$SANDBOX/backup.err"
        check "fat32: reading through the backup exits 0 without a warning" test ! -s "$SANDBOX/backup.err"
        check "fat32: the backup reads the file" test "$(cat "$SANDBOX/backup")" = "sample fat volume"
    fi
    check "$kind: the image is unchanged" test "$(sha256_of "$img")" = "$before"
done

finish
