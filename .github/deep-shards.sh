#!/usr/bin/env bash
# Shards of the deep property-test job in `.github/workflows/ci.yml`. The job's
# matrix is generated from the table below, so this is the only list.
#
#   deep-shards.sh matrix        JSON list of shard names; fails if a test file has no shard
#   deep-shards.sh check SHARD   checks that each test of SHARD's file is in exactly one shard
#   deep-shards.sh run SHARD     runs SHARD's tests in release mode (set PROPTEST_CASES)
#
# Each table line is: shard name, crate directory, test file in `<crate>/tests`,
# then libtest name filters. Filters match substrings, so anchor them (`inv09_`,
# not `inv9`); none means the whole file. The filter `--rest` selects every test
# of the file that no other shard of the file selects, so a new test always runs
# somewhere; give it a shard of its own group if it is slow. Every test file of
# the model needs a shard; other crates list only the files worth a deep run.
#
# The invariant groups are spread so that the shards take about the same time;
# the measurements are in the comment on the `deep` job.
set -euo pipefail

SHARDS='
invariants-1    model invariants      inv01_ inv04_ inv07_
invariants-2    model invariants      inv02_ inv03_ inv13_
invariants-3    model invariants      inv05_ inv14_ inv16_
invariants-4    model invariants      inv08_ inv21_
invariants-5    model invariants      inv09_ inv12_
invariants-6    model invariants      inv10_ inv17_ inv18_
invariants-7    model invariants      inv06_ inv19_ inv20_ inv22_
invariants-8    model invariants      --rest
edge_cases      model edge_cases
rules           model rules
open_questions  model open_questions
model           model model
log-properties  log   properties
log-fuzz-inputs log   fuzz_inputs
'

cd "$(dirname "$0")/.."

rows() { grep -v '^[[:space:]]*$' <<<"$SHARDS"; }

crate_of() {
    rows | awk -v s="$1" '$1 == s { print $2; found = 1 } END { exit !found }' ||
        { echo "unknown shard: ${1:-}" >&2; exit 2; }
}

file_of() { crate_of "$1" >/dev/null && rows | awk -v s="$1" '$1 == s { print $3 }'; }

# The package name of a crate directory, from its manifest.
package_of() { sed -n 's/^name = "\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1; }

# The shard's own table filters, space-separated.
own_filters() { rows | awk -v s="$1" '$1 == s { for (i = 4; i <= NF; i++) printf "%s ", $i }'; }

# The libtest arguments that select the shard's tests (no filter contains a space).
filters_of() {
    local shard=$1 crate file name f
    crate=$(crate_of "$shard")
    file=$(file_of "$shard")
    case " $(own_filters "$shard")" in
    *" --rest "*)
        # Skip everything the file's other shards select.
        for name in $(rows | awk -v c="$crate" -v t="$file" -v s="$shard" '$2 == c && $3 == t && $1 != s { print $1 }'); do
            for f in $(own_filters "$name"); do printf -- '--skip %s ' "$f"; done
        done
        ;;
    *) own_filters "$shard" ;;
    esac
}

cargo_test() {
    local crate=$1
    shift
    cargo test --locked --release -p "$(package_of "$crate")" "$@"
}

# Names of the tests the arguments select, sorted.
list() {
    local crate=$1 file=$2
    shift 2
    cargo_test "$crate" --quiet --test "$file" -- --list "$@" | sed -n 's/: test$//p' | sort
}

case "${1:-}" in
matrix)
    for path in model/tests/*.rs; do
        file=$(basename "$path" .rs)
        rows | awk -v t="$file" '$2 == "model" && $3 == t { found = 1 } END { exit !found }' ||
            { echo "model/tests/$file.rs has no deep shard" >&2; exit 1; }
    done
    for name in $(rows | awk '{ print $1 }'); do
        [[ -f "$(crate_of "$name")/tests/$(file_of "$name").rs" ]] ||
            { echo "shard $name names a missing test file" >&2; exit 1; }
    done
    rows | awk 'BEGIN { printf "[" } { printf "%s\"%s\"", (NR > 1 ? "," : ""), $1 } END { print "]" }'
    ;;
check)
    crate=$(crate_of "${2:-}")
    file=$(file_of "$2")
    all=$(list "$crate" "$file")
    [[ -n $all ]] || { echo "no tests listed for $crate/tests/$file.rs" >&2; exit 1; }
    selected=
    for name in $(rows | awk -v c="$crate" -v t="$file" '$2 == c && $3 == t { print $1 }'); do
        # shellcheck disable=SC2046 # filters are split on spaces on purpose
        one=$(list "$crate" "$file" $(filters_of "$name"))
        echo "$name: $(grep -c . <<<"$one" || true) tests"
        selected+="$one"$'\n'
    done
    selected=$(grep . <<<"$selected" | sort || true)
    missing=$(comm -23 <(echo "$all") <(uniq <<<"$selected"))
    twice=$(uniq -d <<<"$selected")
    [[ -z $missing ]] || printf 'In no shard of %s:\n%s\n' "$file" "$missing" >&2
    [[ -z $twice ]] || printf 'In more than one shard of %s:\n%s\n' "$file" "$twice" >&2
    [[ -z $missing && -z $twice ]] || exit 1
    echo "$crate/tests/$file.rs: all $(grep -c . <<<"$all") tests, each in exactly one shard"
    ;;
run)
    crate=$(crate_of "${2:-}")
    file=$(file_of "$2")
    # shellcheck disable=SC2046 # filters are split on spaces on purpose
    cargo_test "$crate" --test "$file" -- $(filters_of "$2")
    ;;
*)
    sed -n '5,7s/^#  *//p' "$0" >&2
    exit 2
    ;;
esac
