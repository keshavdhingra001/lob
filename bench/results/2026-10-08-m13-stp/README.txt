M13 STP cost, 2026-10-08. `lob bench` (best of 5 per run), taskset -c 2, gated by scripts/quiet.sh (idle 94-95%).
throughput-m12-vs-m13.txt: gen2m.jrnl (2M commands, no groups), M12 binary (374cc64) and M13 binary alternated, 3 rounds.
  "old run 3" fast was disturbed (17 M/s, outside the others by 40%) and is excluded.
throughput-grouped.txt: lob gen 1 2000000 ... 5000 3 (3 STP groups), M13 binary, 3 runs.
