#!/usr/bin/env bash
# Profiling (D29): hardware counters per command and a sampled profile, both books.
#
# Usage: scripts/profile.sh [cpu] [journal]   (defaults: cpu 2, target/latency/gen2m.jrnl)
#
# Needs `perf` (Arch: `sudo pacman -S perf`). With kernel.perf_event_paranoid = 2 a normal
# user can count and sample their own process in user space, so every event is `:u`.
# `lob run none` only decodes the journal; subtract it to get matching alone.
set -euo pipefail
cd "$(dirname "$0")/.."
cpu="${1:-2}"
journal="${2:-target/latency/gen2m.jrnl}"
repeats=5
export TMPDIR="$PWD/target/tmp"
mkdir -p "$TMPDIR" target/prof

cargo build --release -q
cargo build --profile profiling -q
[ -f "$journal" ] || target/release/lob gen 1 2000000 "$journal" >/dev/null
commands=$(target/release/lob run none "$journal" 1 | cut -d' ' -f1)

events=cycles:u,instructions:u,branches:u,branch-misses:u,cache-references:u,cache-misses:u,L1-dcache-load-misses:u,dTLB-load-misses:u
declare -A base
echo "per command, decode-only baseline subtracted ($commands commands x $repeats, perf stat -r 3)"
for book in none ref fast; do
  out=$(taskset -c "$cpu" perf stat -x, -r 3 -e "$events" target/release/lob run "$book" "$journal" "$repeats" 2>&1 >/dev/null)
  [ "$book" = none ] || echo "--- $book"
  while IFS=, read -r value _ name _; do
    if [ "$book" = none ]; then
      base[$name]=$value
    else
      awk -v v="$value" -v b="${base[$name]}" -v n="$((commands * repeats))" -v name="$name" \
        'BEGIN { printf "%-24s %8.2f\n", name, (v - b) / n }'
    fi
  done <<< "$out"
done

for book in ref fast; do
  taskset -c "$cpu" perf record -q -F 4999 --call-graph dwarf,16384 -o "target/prof/$book.data" \
    target/profiling/lob run "$book" "$journal" 2 >/dev/null 2>&1
  echo "=== $book: self time by function (perf report -i target/prof/$book.data for more)"
  perf report -i "target/prof/$book.data" --no-children --stdio --sort symbol --percent-limit 1.5 -g none 2>/dev/null \
    | grep -E '^ +[0-9]' | cut -c1-120
done
