# tests/cli/lib.sh -- what every tests/cli/test-*.sh sources.
#
# The house style: `ok`/`fail`, a `fails` counter, `set -uo pipefail` (not -e,
# so later checks still run after one fails), a sandbox under the repository's
# tmp/, and `finish` last, which prints the count scripts/test-cli.sh sums and
# the trailing `<name>: all checks passed` line it requires.
#
# The tools are whatever PATH finds: scripts/test-cli.sh has already made sure
# they are ours before any file runs.
set -uo pipefail

NAME="$(basename "$0" .sh)"
REPO="$(cd "$(dirname "$0")/../.." && pwd -P)"
CRATE="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)"

passed=0
fails=0
ok() { passed=$((passed + 1)); }
fail() {
    echo "FAIL  $NAME: $*" >&2
    fails=$((fails + 1))
}

# check DESCRIPTION COMMAND...: ok if COMMAND succeeds, else fail naming it.
check() {
    local what="$1"
    shift
    if "$@"; then ok; else fail "$what"; fi
}

# jq_check DESCRIPTION FILTER FILE: ok if jq -e FILTER holds for FILE.
jq_check() {
    local what="$1" filter="$2" file="$3"
    if jq -e "$filter" "$file" >/dev/null 2>&1; then
        ok
    else
        fail "$what: jq -e '$filter' is false for: $(head -c 2000 "$file")"
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/cli-$NAME.XXXXXX")" || exit 1
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

# sample_image FAT12|FAT32: a fresh copy of a committed sample volume in the
# sandbox, its path printed. They were formatted by mkfs.fat and filled by
# mtools (validation/make-cli-samples.sh); samples.sha256 holds the digest of
# each uncompressed volume, so a stale or altered file fails here.
sample_image() {
    local kind="$1" image="$SANDBOX/sample-$1.img" want
    gzip -dc "$REPO/tests/cli/sample-$kind.img.gz" >"$image" || {
        echo "FAIL  $NAME: cannot decompress tests/cli/sample-$kind.img.gz" >&2
        exit 1
    }
    want="$(awk -v n="sample-$kind.img" '$2 == n { print $1 }' "$REPO/tests/cli/samples.sha256")"
    [ "$(sha256_of "$image")" = "$want" ] || {
        echo "FAIL  $NAME: tests/cli/sample-$kind.img.gz is not the volume samples.sha256 names" >&2
        exit 1
    }
    printf '%s\n' "$image"
}

finish() {
    if [ "$fails" -gt 0 ]; then
        echo "test result: FAILED. $passed passed; $fails failed"
        exit 1
    fi
    echo "test result: ok. $passed passed; 0 failed"
    echo "$NAME: all checks passed"
    exit 0
}
