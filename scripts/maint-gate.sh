#!/usr/bin/env bash
# Roadmap 6.2a2b: the gate for a fourth thread (compaction design §4, Q-801) and ingest latency with and without a
# running background task, on one core.
#
#   scripts/maint-gate.sh <dir> [--secs <n>] [--base <rev>] [--no-gate]
#
# <dir> must be on the file system under test (not tmpfs). Two device classes are stood in for by a sleep in every
# sync: an SSD (2 ms, a task rate of 8 MiB/s) and a card (25 ms, 1 MiB/s). With --base, the commit-latency harness
# also runs its steady loads on <rev> and on the working tree, without a task, to compare ingest latency.
# Output goes to stdout in Markdown; copy it into docs/measurements/6.2a2b-maintenance-gate.md.
set -euo pipefail
if [ $# -lt 1 ]; then
  echo "usage: scripts/maint-gate.sh <dir> [--secs <n>] [--base <rev>] [--no-gate]" >&2
  exit 2
fi
dir=$(cd "$1" && pwd)
shift
secs=60
base=
gate=1
while [ $# -gt 0 ]; do
  case "$1" in
    --secs) secs=$2; shift 2 ;;
    --base) base=$2; shift 2 ;;
    --no-gate) gate=; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
cargo build --release --locked -p kvik-log --example maint_gate --example commit_latency >&2
for class in ${gate:+"2 8" "25 1"}; do
  set -- $class
  echo "### Sync $1 ms, task rate $2 MiB/s"
  echo
  taskset -c 0 target/release/examples/maint_gate "$dir" --sync-delay "$1" --io-rate "$2" --secs "$secs"
  echo
done
if [ -n "$base" ]; then
  wt=$(mktemp -d)
  git worktree add --detach "$wt/base" "$base" >&2
  (cd "$wt/base" && cargo build --release --locked -p kvik-log --example commit_latency >&2)
  for tree in "$wt/base" "$root"; do
    for run in 1 2 3; do
      echo "### Commit latency, $( [ "$tree" = "$root" ] && echo head || echo "base $base" ), run $run"
      echo
      taskset -c 0 "$tree/target/release/examples/commit_latency" "$dir" --short --only steady \
        | grep -E '^\| (Load|---|steady)' || true
    done
  done
  git worktree remove --force "$wt/base" >&2
  rm -rf "$wt"
fi
