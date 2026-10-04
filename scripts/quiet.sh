#!/usr/bin/env bash
# Wait until the machine is quiet enough to benchmark: CPU idle >= MIN% (default 85) in
# two consecutive 2-second samples, whatever else is running. Gives up after MAX seconds.
# Usage: scripts/quiet.sh [min-idle-percent] [max-wait-seconds]
min="${1:-85}"
max="${2:-1800}"
start=$SECONDS
ok=0
while [ $((SECONDS - start)) -lt "$max" ]; do
  idle=$(vmstat 2 2 | tail -1 | awk '{print $15}')
  if [ "$idle" -ge "$min" ]; then ok=$((ok + 1)); else ok=0; fi
  if [ "$ok" -ge 2 ]; then echo "quiet (idle ${idle}%)"; exit 0; fi
done
echo "not quiet after ${max}s (idle ${idle}%)" >&2
exit 1
