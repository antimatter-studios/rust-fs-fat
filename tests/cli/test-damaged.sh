# A damaged boot sector: info says what is wrong and where the backup is, the
# other verbs refuse with 4, and --boot-sector reads the volume from the
# backup; none of it writes.
source "$(dirname "$0")/lib.sh"

img="$(sample_image fat32)"
dd if=/dev/zero of="$img" bs=512 count=1 conv=notrunc status=none
before="$(sha256_of "$img")"

fs.fat "$img" info >"$SANDBOX/info.json" 2>"$SANDBOX/info.err"
check "info of a zeroed sector 0 exits 0 ($(cat "$SANDBOX/info.err"))" test $? -eq 0
jq_check "info says the boot sector is not valid and why" \
    '.valid == false and (.problem | test("signature")) and .volume == null' "$SANDBOX/info.json"
jq_check "info finds the FAT32 backup at sector 6 and says how to use it" \
    '.backup.accepted == true and .backup.sector == 6 and .backup.suggest == "--boot-sector 6"' "$SANDBOX/info.json"

fs.fat "$img" ls / >"$SANDBOX/refused.out" 2>"$SANDBOX/refused.err"
check "ls on a damaged boot sector exits 4" test $? -eq 4
check "a refusal prints nothing on stdout" test ! -s "$SANDBOX/refused.out"
jq_check "the refusal names --boot-sector" '.code == 4 and (.error | contains("--boot-sector"))' "$SANDBOX/refused.err"

fs.fat "$img" --boot-sector 6 ls -R / >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"
check "ls through the backup exits 0 ($(cat "$SANDBOX/ls.err"))" test $? -eq 0
jq_check "ls through the backup lists the volume" '[.[].path] | index("/docs/deep/leaf.bin") != null' "$SANDBOX/ls.json"
fs.fat "$img" --boot-sector 6 cat /README.TXT >"$SANDBOX/readme"
check "cat through the backup reads the file" test "$(cat "$SANDBOX/readme")" = "sample fat volume"
check "the damaged image is unchanged" test "$(sha256_of "$img")" = "$before"

finish
