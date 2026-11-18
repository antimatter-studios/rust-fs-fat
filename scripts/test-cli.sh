#!/usr/bin/env bash
# test-cli.sh -- THE `cli` TIER: the command-line tool as installed, found on
# PATH, not a cargo target.
#
# The same suite checks a checkout's build (`chore cli:install` stages it and
# prints the PATH line) and a `cargo install rust-fs-fat32 --features cli`: it
# tests whatever PATH resolves, because that is what a user runs.
#
# STEP 1, BEFORE ANY CHECK: the tool is present and is ours.
#   - jq and rust-fs-fat must resolve, and rust-fs-fat must answer --version
#     as this crate;
#   - `rust-fs-fat doctor` must pass: every dotted name on PATH is our program
#     at our version. A shadowed name fails the tier with doctor's report:
#     what wins, and the fix.
# NOTHING SKIPS. A missing tool fails the tier naming what provides it.
#
# STEP 2: every tests/cli/test-*.sh, by glob, so a new one needs no edit
# here. Each prints its failures, a `test result: ok. N passed; ...` line, and
# LAST `<name>: all checks passed`. A file that exits 0 without that last
# line stopped early and is a failure.
#
# STEP 3, THE FLOOR: the passes are summed, and a run that executed fewer
# checks than CLI_FLOOR fails even when every file passed: a file that quietly
# stops running its checks reads exactly like one that passes. The floor is
# about 95% of what the tier executed when it was last raised (113).
#
# CLI_TESTS names another directory of test-*.sh files and CLI_FLOOR another
# floor, for a test of this script itself.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TESTS="${CLI_TESTS:-$REPO/tests/cli}"
FLOOR="${CLI_FLOOR:-107}"
CRATE="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)"
INSTALL="build and stage it with \`chore cli:install\` (it prints the PATH line to use), or install it with \`cargo install $CRATE --features cli\`"

refuse() {
    echo "test-cli: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 ||
    refuse "jq is not on PATH; the suite reads every JSON report with it. Install it: \`brew install jq\`, or \`apt-get install jq\`."
command -v gzip >/dev/null 2>&1 || refuse "gzip is not on PATH; the sample volumes are gzipped"

entry="$(command -v rust-fs-fat 2>/dev/null || true)"
[ -n "$entry" ] || refuse "rust-fs-fat is not on PATH: $INSTALL."
answer="$(rust-fs-fat --version 2>/dev/null || true)"
case "$answer" in
    "rust-fs-fat ($CRATE) "*) ;;
    *) refuse "$entry is not $CRATE's rust-fs-fat: --version answered '$answer'. Put ours first on PATH: $INSTALL." ;;
esac
echo "== $answer, at $entry"

echo "== rust-fs-fat doctor"
if ! report="$(rust-fs-fat doctor --text 2>&1)"; then
    printf '%s\n' "$report" >&2
    refuse "doctor found a tool on PATH that is not this program; its fixes are above. Nothing was tested."
fi
printf '%s\n' "$report"

shopt -s nullglob
files=("$TESTS"/test-*.sh)
[ "${#files[@]}" -gt 0 ] || refuse "no test-*.sh in $TESTS to run"

failed=0
executed=0
for file in "${files[@]}"; do
    name="$(basename "$file" .sh)"
    echo "== $name"
    output="$(bash "$file" 2>&1)"
    status=$?
    printf '%s\n' "$output"
    last="$(printf '%s\n' "$output" | tail -n 1)"
    if [ "$status" -ne 0 ]; then
        echo "FAIL  $name exited $status" >&2
        failed=$((failed + 1))
    elif [ "$last" != "$name: all checks passed" ]; then
        echo "FAIL  $name exited 0 without its last line, '$name: all checks passed': it stopped early" >&2
        failed=$((failed + 1))
    fi
    passed="$(printf '%s\n' "$output" | sed -n 's/^test result: ok\. \([0-9][0-9]*\) passed.*/\1/p' | tail -n 1)"
    executed=$((executed + ${passed:-0}))
done

if [ "$failed" -gt 0 ]; then
    echo "test-cli: $failed of ${#files[@]} files failed" >&2
    exit 1
fi
if [ "$executed" -lt "$FLOOR" ]; then
    echo "test-cli: only $executed checks executed, and the floor is $FLOOR: every file passed, so something stopped checks from running rather than failing them" >&2
    exit 1
fi
echo "test-cli: ${#files[@]} files, $executed checks (floor $FLOOR), all checks passed"
