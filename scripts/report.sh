#!/usr/bin/env bash
# The M11 benchmark report (D53–D56): every measurement in one session, raw output to
# bench/results/<date-time>/. BENCHMARKS.md is written by hand from those files.
#
# Usage: scripts/report.sh [cpu] [itch-file]   (defaults: cpu 2, data/07302019.NASDAQ_ITCH50)
#
# Each part first waits for a quiet machine (scripts/quiet.sh). Single-thread parts are
# pinned to `cpu`; the pipeline gets `cpu-1,cpu,cpu+1` (three physical cores on a 4-core,
# 8-thread laptop where cpu n and n+4 share a core). Without the ITCH file, the ITCH parts
# are skipped. A full run takes about 15 minutes.
#
# To redo parts that something disturbed, into an existing results folder:
#   OUT=bench/results/<date-time> ONLY="latency-gen2m criterion" scripts/report.sh
# Their new output is appended after the old, so both stay on record. Without OUT, ONLY
# measures just those parts into a new folder (M12's latency plots were made that way).
set -euo pipefail
cd "$(dirname "$0")/.."
cpu="${1:-2}"
itch="${2:-data/07302019.NASDAQ_ITCH50}"
export TMPDIR="$PWD/target/tmp"
out="${OUT:-bench/results/$(date +%F-%H%M)}"
only="${ONLY:-}"
mkdir -p "$TMPDIR" target/latency "$out"

# Build everything first: a build uses every core, and must not overlap a measurement.
cargo build --release -q
cargo bench -q --bench book --no-run 2>/dev/null
lob=target/release/lob

# What every number was measured on (D55). A folder gets this once, from its first run.
[ -f "$out/machine.txt" ] || {
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

# Wait for a quiet machine, and record how quiet it was in the part's file.
quiet() {
  scripts/quiet.sh 85 900 >> "$1" 2>&1 || echo "WARNING: the machine never got quiet; treat these numbers with care" >> "$1"
}

# Run one part: `part <name> <command...>`, appending its output to $out/<name>.txt.
part() {
  local name="$1"
  shift
  if [ -n "$only" ] && [[ " $only " != *" $name "* ]]; then
    return 0
  fi
  echo "== $name: $*" | tee -a "$out/$name.txt"
  quiet "$out/$name.txt"
  "$@" >> "$out/$name.txt" 2>&1
  # A load average well above 1 here means something else ran during the part.
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
    if [ -z "$only" ] || [ ! -f "target/latency/$j.jrnl" ]; then
      part translate $lob itch "$itch" journal "$s" "target/latency/$j.jrnl"
    fi
    journals+=("$j")
  done
else
  echo "no ITCH file at $itch: skipping the ITCH parts"
fi

for j in "${journals[@]}"; do
  part throughput taskset -c "$cpu" $lob bench "target/latency/$j.jrnl"
done
for j in "${journals[@]}"; do
  part "latency-$j" taskset -c "$cpu" $lob latency "target/latency/$j.jrnl" 5 "$out/hgrm/$j"
done
# Percentile plots of the latency parts that ran (D60). Drawing needs no quiet machine.
for j in "${journals[@]}"; do
  h="$out/hgrm/$j"
  if [ -f "$h/fast.hgrm" ]; then
    $lob plot "$out/latency-$j.svg" "$j: latency per command, median-p99 run of 5" \
      "reference book=$h/ref.hgrm" "fast book=$h/fast.hgrm" "clock floor=$h/clock.hgrm"
  fi
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
