#!/usr/bin/env bash
# Ingest latency before and after a change (Q-2184): runs `benches/emit.rs` of this checkout against BASE (a
# revision, built in a temporary worktree with this bench copied in) and HEAD, alternating, RUNS times each.
#   scripts/bench-emit-ab.sh BASE OUT [RUNS]
set -euo pipefail
base=$1 out=$2 runs=${3:-3}
root=$(git rev-parse --show-toplevel)
wt=$(mktemp -d)
trap 'git -C "$root" worktree remove --force "$wt" >/dev/null 2>&1 || true' EXIT
git -C "$root" worktree add --detach "$wt" "$base" >/dev/null
cp "$root/kvik/benches/emit.rs" "$wt/kvik/benches/emit.rs"
grep -q 'name = "emit"' "$wt/kvik/Cargo.toml" ||
  sed -i 's/^\[lints\]/[[bench]]\nname = "emit"\nharness = false\n\n[lints]/' "$wt/kvik/Cargo.toml"
# Each side's executable is the path cargo prints; the base builds into its own target directory under this one.
build() {
  (cd "$1" && CARGO_TARGET_DIR=$2 cargo bench -p kvik --bench emit --no-run --locked 2>&1) |
    sed -n 's/.*Executable .*(\(.*\)).*/\1/p' | tail -1 |
    { read -r p; case $p in /*) echo "$p" ;; *) echo "$1/$p" ;; esac; }
}
b=$(build "$wt" "$root/target/bench-base")
h=$(build "$root" "$root/target")
[ -x "$b" ] && [ -x "$h" ] || { echo "build failed" >&2; exit 1; }
{
  echo "# Ingest latency through emit, $(git -C "$root" rev-parse --short "$base") against $(git -C "$root" rev-parse --short HEAD)"
  echo
  echo "Host: $(nproc) cores; \`scripts/bench-emit-ab.sh $base <out> $runs\`; runs alternate base, head."
  echo
  for i in $(seq "$runs"); do
    for side in base head; do
      [ $side = base ] && exe=$b || exe=$h
      echo "## $side, run $i"
      echo
      echo '```'
      "$exe" --bench
      echo '```'
      echo
    done
  done
} > "$out"
