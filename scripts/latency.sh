#!/usr/bin/env bash
# Latency method (D27): release build, generated journals, pinned to one core,
# warm-up + N runs per book inside `lob latency`, machine details printed with the results.
#
# Usage: scripts/latency.sh [cpu] [runs]     (defaults: cpu 2, 5 runs)
#
# cpu 0 takes most device interrupts on Linux, so the default is 2. For the quietest
# numbers, also keep the cpu's SMT sibling idle (see thread_siblings_list below) and,
# if you can, switch the governor to performance (needs root; this script never does).
set -euo pipefail
cd "$(dirname "$0")/.."
cpu="${1:-2}"
runs="${2:-5}"
export TMPDIR="$PWD/target/tmp"
mkdir -p "$TMPDIR" target/latency

cargo build --release -q
lob=target/release/lob

# The journals are deterministic (seeded), so regenerating them is only a time cost.
[ -f target/latency/gen2m.jrnl ] || $lob gen 1 2000000 target/latency/gen2m.jrnl >/dev/null
[ -f target/latency/deep200k.jrnl ] || $lob gen 1 2000000 target/latency/deep200k.jrnl 200000 >/dev/null
[ -f target/latency/queue10k.jrnl ] || $lob gen-queue 10000 target/latency/queue10k.jrnl >/dev/null

echo "pinned to cpu $cpu (SMT siblings: $(cat /sys/devices/system/cpu/cpu"$cpu"/topology/thread_siblings_list 2>/dev/null || echo '?'))"
for j in gen2m deep200k queue10k; do
  echo "=== $j ==="
  taskset -c "$cpu" $lob latency "target/latency/$j.jrnl" "$runs"
done
