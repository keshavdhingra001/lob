#!/usr/bin/env bash
# The M11 benchmark report (D53–D56): every measurement in one session, raw output to
# bench/results/<date-time>/. BENCHMARKS.md is written by hand from those files.
#
# Usage: scripts/report.sh [cpu] [itch-file]   (defaults: cpu 2, data/07302019.NASDAQ_ITCH50)
#
# Each part first waits for a quiet machine (scripts/quiet.sh). Single-thread parts are
# pinned to `cpu`; the pipeline gets `cpu-1,cpu,cpu+1` (three physical cores on a 4-core,
# 8-thread laptop where cpu n and n+4 share a core). Without the ITCH file, the ITCH parts
# are skipped. A full run takes about 30 minutes.
set -euo pipefail
cd "$(dirname "$0")/.."
cpu="${1:-2}"
itch="${2:-data/07302019.NASDAQ_ITCH50}"
export TMPDIR="$PWD/target/tmp"
out="bench/results/$(date +%F-%H%M)"
mkdir -p "$TMPDIR" target/latency "$out"

cargo build --release -q
lob=target/release/lob

# What every number was measured on (D55).
{
  echo "date     $(date -Is)"
  echo "commit   $(git rev-parse --short HEAD)$(git diff --quiet HEAD -- src benches scripts Cargo.toml || echo ' (modified)')"
  echo "rustc    $(rustc -V)"
  echo "cpu      $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
  echo "kernel   $(uname -r)"
  echo "governor $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo '?')" \
    "(epp $(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null || echo '?'))"
  echo "memory   $(free -g | awk '/Mem:/ {print $2 " GB"}')"
  echo "pinned   cpu $cpu (SMT siblings: $(cat /sys/devices/system/cpu/cpu"$cpu"/topology/thread_siblings_list 2>/dev/null || echo '?'))"
} > "$out/machine.txt"
cat "$out/machine.txt"

# Wait for a quiet machine, and record how quiet it was in the part's file.
quiet() {
  scripts/quiet.sh 85 900 >> "$1" 2>&1 || echo "WARNING: the machine never got quiet; treat these numbers with care" >> "$1"
}

# Run one part: `part <name> <command...>`, appending its output to $out/<name>.txt.
part() {
  local name="$1"
  shift
  echo "== $name: $*" | tee -a "$out/$name.txt"
  quiet "$out/$name.txt"
  "$@" >> "$out/$name.txt" 2>&1
  echo "uptime: $(uptime)" >> "$out/$name.txt"
}

# Journals: generated ones are seeded, so regenerating them is only a time cost.
[ -f target/latency/gen2m.jrnl ] || $lob gen 1 2000000 target/latency/gen2m.jrnl >/dev/null
[ -f target/latency/deep200k.jrnl ] || $lob gen 1 2000000 target/latency/deep200k.jrnl 200000 >/dev/null
[ -f target/latency/queue10k.jrnl ] || $lob gen-queue 10000 target/latency/queue10k.jrnl >/dev/null
journals=(gen2m deep200k queue10k)
if [ -f "$itch" ]; then
  for s in AAPL SPY; do
    j=$(echo "$s" | tr 'A-Z' 'a-z')
    part translate $lob itch "$itch" journal "$s" "target/latency/$j.jrnl"
    journals+=("$j")
  done
else
  echo "no ITCH file at $itch: skipping the ITCH parts"
fi

for j in "${journals[@]}"; do
  part throughput taskset -c "$cpu" $lob bench "target/latency/$j.jrnl"
done
for j in "${journals[@]}"; do
  part "latency-$j" taskset -c "$cpu" $lob latency "target/latency/$j.jrnl" 5
done
for j in gen2m aapl spy; do
  [ -f "target/latency/$j.jrnl" ] && part feed taskset -c "$cpu" $lob feed "target/latency/$j.jrnl"
done
cores="$((cpu - 1)),$cpu,$((cpu + 1))"
for rate in 0 1000000; do
  for channel in ring mpsc; do
    part pipeline taskset -c "$cores" $lob pipeline target/latency/gen2m.jrnl "$rate" "$channel"
  done
done
if [ -f "$itch" ]; then
  for mode in frame decode book; do
    part itch taskset -c "$cpu" $lob itch "$itch" "$mode"
  done
fi
part criterion taskset -c "$cpu" cargo bench -q --bench book
echo "done: $out"
