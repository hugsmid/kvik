#!/usr/bin/env bash
# Shards of the `test` job in `.github/workflows/ci.yml`, one job per OS and shard.
#
#   test-shards.sh matrix        JSON list of shard names; fails unless check passes
#   test-shards.sh check         checks that each default member is in exactly one shard, and lists them
#   test-shards.sh run SHARD     runs `cargo test` on SHARD's packages (set PROPTEST_CASES)
#
# Each table line is: shard name, then crate directories of default members. A shard
# runs every target of its packages (unit, integration and doc tests), so a package in
# exactly one shard runs each of its tests exactly once. The filter `--rest` selects every
# default member no other shard names, so a new crate always runs somewhere.
#
# Windows test time per package at 1,024 cases (the step summed about 28 minutes before
# the split, build excluded): kvik 632 s, kvik-log 596 s, kvik-model 388 s, the rest 35 s.
# Rebalance when a shard's job nears 20 minutes.
set -euo pipefail

SHARDS='
kvik    kvik
log     log
model   model --rest
'

cd "$(dirname "$0")/.."

rows() { grep -v '^[[:space:]]*$' <<<"$SHARDS"; }

# The default members' crate directories, from the workspace manifest.
members() { sed -n 's/^default-members = \[\(.*\)\]$/\1/p' Cargo.toml | tr -d '" ' | tr ',' '\n' | grep .; }

named() { rows | awk '{ for (i = 2; i <= NF; i++) if ($i != "--rest") print $i }'; }

# The crate directories of a shard.
crates_of() {
    local c
    rows | awk -v s="${1:-}" '$1 == s { found = 1 } END { exit !found }' ||
        { echo "unknown shard: ${1:-}" >&2; exit 2; }
    for c in $(rows | awk -v s="$1" '$1 == s { for (i = 2; i <= NF; i++) print $i }'); do
        if [[ $c == --rest ]]; then
            comm -23 <(members | sort) <(named | sort)
        else
            echo "$c"
        fi
    done
}

package_of() { sed -n 's/^name = "\(.*\)"$/\1/p' "$1/Cargo.toml" | head -n 1; }

check() {
    local all selected missing twice extra
    all=$(members | sort)
    [[ -n $all ]] || { echo "no default members in Cargo.toml" >&2; exit 1; }
    selected=
    for name in $(rows | awk '{ print $1 }'); do
        one=$(crates_of "$name")
        echo "$name: $(tr '\n' ' ' <<<"$one")" >&2
        selected+="$one"$'\n'
    done
    selected=$(grep . <<<"$selected" | sort)
    missing=$(comm -23 <(echo "$all") <(uniq <<<"$selected"))
    extra=$(comm -13 <(echo "$all") <(uniq <<<"$selected"))
    twice=$(uniq -d <<<"$selected")
    [[ -z $missing ]] || printf 'In no shard:\n%s\n' "$missing" >&2
    [[ -z $extra ]] || printf 'Not a default member:\n%s\n' "$extra" >&2
    [[ -z $twice ]] || printf 'In more than one shard:\n%s\n' "$twice" >&2
    [[ -z $missing && -z $extra && -z $twice ]] || exit 1
    echo "all $(grep -c . <<<"$all") default members, each in exactly one shard" >&2
}

case "${1:-}" in
matrix)
    check
    rows | awk 'BEGIN { printf "[" } { printf "%s\"%s\"", (NR > 1 ? "," : ""), $1 } END { print "]" }'
    ;;
check) check ;;
run)
    crates=$(crates_of "${2:-}")
    [[ -n $crates ]] || { echo "shard ${2:-} has no packages" >&2; exit 1; }
    args=()
    for c in $crates; do args+=(-p "$(package_of "$c")"); done
    cargo test --locked "${args[@]}"
    ;;
*)
    sed -n '4,6s/^#  *//p' "$0" >&2
    exit 2
    ;;
esac
