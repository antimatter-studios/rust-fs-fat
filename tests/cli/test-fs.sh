# fs.fat on the two sample volumes, as installed: every read verb, the
# chains mshowfat recorded, and that the source is never written.
source "$(dirname "$0")/lib.sh"

for kind in fat12 fat32; do
    img="$(sample_image "$kind")"
    before="$(sha256_of "$img")"
    manifest="$REPO/tests/cli/sample-$kind.manifest"

    fs.fat "$img" info >"$SANDBOX/info.json" 2>"$SANDBOX/info.err"
    check "$kind: info exits 0 ($(cat "$SANDBOX/info.err"))" test $? -eq 0
    jq_check "$kind: info says valid, $(echo "$kind" | tr a-z A-Z)" \
        ".valid == true and .volume.variant == \"$(echo "$kind" | tr a-z A-Z)\"" "$SANDBOX/info.json"

    fs.fat "$img" ls -R / >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"
    check "$kind: ls -R / exits 0 ($(cat "$SANDBOX/ls.err"))" test $? -eq 0
    jq_check "$kind: ls -R lists the six entries mtools wrote, not the deleted one" \
        '[.[].path] | sort == ["/README.TXT","/docs","/docs/Guide With Spaces.txt","/docs/deep","/docs/deep/leaf.bin","/empty.dat"]' "$SANDBOX/ls.json"
    jq_check "$kind: docs is a directory" '.[] | select(.path == "/docs") | .type == "directory"' "$SANDBOX/ls.json"
    jq_check "$kind: the long name keeps its size" '.[] | select(.name == "Guide With Spaces.txt") | .size == 1300' "$SANDBOX/ls.json"
    fs.fat "$img" ls /docs/DEEP >"$SANDBOX/deep.json"
    jq_check "$kind: a path matches without regard to case" '[.[].name] == ["leaf.bin"]' "$SANDBOX/deep.json"

    fs.fat "$img" stat /docs/deep/leaf.bin >"$SANDBOX/stat.json"
    jq_check "$kind: stat reports size 2000" '.size == 2000 and .type == "file"' "$SANDBOX/stat.json"

    fs.fat "$img" cat /README.TXT >"$SANDBOX/readme"
    check "$kind: cat writes the file's bytes" test "$(cat "$SANDBOX/readme")" = "sample fat volume"
    fs.fat "$img" cat /empty.dat >"$SANDBOX/empty"
    check "$kind: cat of an empty file writes nothing" test ! -s "$SANDBOX/empty"

    fs.fat "$img" export /docs/deep/leaf.bin "$SANDBOX/leaf-$kind" --verify >"$SANDBOX/export.json" 2>"$SANDBOX/export.err"
    check "$kind: export --verify exits 0 ($(cat "$SANDBOX/export.err"))" test $? -eq 0
    jq_check "$kind: export reports a verified copy of 2000 bytes" '.verified == true and .bytes == 2000' "$SANDBOX/export.json"
    check "$kind: export's digest is the exported file's" test "$(jq -r .sha256 "$SANDBOX/export.json")" = "$(sha256_of "$SANDBOX/leaf-$kind")"
    fs.fat "$img" cat /docs/deep/leaf.bin >"$SANDBOX/leaf.cat-$kind"
    check "$kind: export writes what cat writes" cmp -s "$SANDBOX/leaf-$kind" "$SANDBOX/leaf.cat-$kind"

    # The chains mtools' mshowfat recorded.
    while IFS=$'\t' read -r tag path clusters; do
        [ "$tag" = chain ] || continue
        first="${clusters%%,*}"
        fs.fat "$img" chain "$first" >"$SANDBOX/chain.json"
        jq_check "$kind: chain from $first is $clusters" \
            "(.clusters | map(tostring) | join(\",\")) == \"$clusters\" and .stopped == \"end_of_chain\"" "$SANDBOX/chain.json"
    done <"$manifest"

    # The deleted record, and what its cleared chain reads as.
    fs.fat "$img" scan --deleted >"$SANDBOX/scan.json"
    jq_check "$kind: scan --deleted finds gone.txt's record, size 700" \
        'length == 1 and .[0].deleted == true and .[0].size == 700 and (.[0].raw | startswith("e5"))' "$SANDBOX/scan.json"
    fs.fat "$img" chain "$(jq -r '.[0].first_cluster' "$SANDBOX/scan.json")" >"$SANDBOX/gone.json"
    jq_check "$kind: the deleted file's cleared chain reads as free" '.stopped == "free"' "$SANDBOX/gone.json"

    fs.fat "$img" cat /absent >"$SANDBOX/absent.out" 2>"$SANDBOX/absent.err"
    check "$kind: cat of a missing path exits 1" test $? -eq 1
    check "$kind: a missing path prints nothing on stdout" test ! -s "$SANDBOX/absent.out"
    jq_check "$kind: a missing path is a structured not_found error" '.kind == "not_found" and .code == 1' "$SANDBOX/absent.err"

    check "$kind: rust-fs-fat fs is the same program" \
        test "$(rust-fs-fat fs --text "$img" ls /)" = "$(fs.fat --text "$img" ls /)"
    check "$kind: the source image is unchanged" test "$(sha256_of "$img")" = "$before"
done

finish
