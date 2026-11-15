#!/usr/bin/env bash
# Roadmap 4.8: the full commit-latency run on the Linux NVMe target, the pending step of
# docs/measurements/4.8-commit-latency.md that checks the delivery budget of docs/design.md.
#
#   scripts/bench-nvme.sh <dir> [--disk "<class>"] [--repeat <n>]
#
# <dir> must be on a file system of the NVMe disk under test (not tmpfs). --disk states what cannot be found out,
# for example "NVMe, power-loss protection" or "NVMe, consumer, no power-loss protection"; never a model or serial.
# --repeat defaults to 3 (about 25 minutes). The results go to docs/measurements/4.8-linux-nvme-<date>-<time>.md, in the
# harness's own format; commit that file and fill in the "Linux NVMe target" section of 4.8-commit-latency.md.
# Run on an otherwise idle machine, at normal priority, with nothing else writing to the disk.
set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
  echo "this run is for Linux on the NVMe target; on other systems run the example directly" >&2
  exit 2
fi
if [ $# -lt 1 ]; then
  echo "usage: scripts/bench-nvme.sh <dir on the NVMe file system> [--disk \"<class>\"] [--repeat <n>]" >&2
  exit 2
fi
dir=$(cd "$1" && pwd)
shift
repeat=3
extra=()
while [ $# -gt 0 ]; do
  case "$1" in
    --repeat) repeat=$2; shift 2 ;;
    --disk) extra+=(--disk "$2"); shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done

cd "$(dirname "$0")/.."
out="docs/measurements/4.8-linux-nvme-$(date -u +%Y-%m-%d-%H%M%S).md"
if [ -e "$out" ]; then
  echo "$out exists; not overwriting it" >&2
  exit 2
fi
tmp="$out.partial"
cargo build --release --locked -p kvik-log --example commit_latency
{
  echo "# 4.8 commit latency: Linux NVMe target"
  echo
  echo "Made by \`scripts/bench-nvme.sh\` at commit $(git rev-parse --short HEAD), $(date -u +%Y-%m-%d)," \
    "$repeat runs of the full matrix. Method and reading: [4.8-commit-latency.md](4.8-commit-latency.md)."
  echo
  target/release/examples/commit_latency "$dir" --repeat "$repeat" ${extra[@]+"${extra[@]}"}
} > "$tmp"
mv "$tmp" "$out"
echo "results written to $out" >&2
