# Amendments to PROTOCOL-REMEASURE.md (sealed 2026-11-01T05:52:49Z, sha256 1be4dc64...)

No change to the registered method so far. Notes on files:

N1 (2026-11-01T07:11Z, note, not a method change). harness/watch.sh is the operator's watcher that polls the node's
logs; it measures nothing. After sealing (sealed sha256 3dbba3d3...), its remote `grep`/`tail` commands were pinned
to CPUs 0-3 with `taskset` so that monitoring never lands on the measured CPUs (new sha256 83b173ce...). Earlier
status checks during step 1 (a few, each under a second) were not pinned.

N2 (2026-11-01T08:49Z, note, not a method change; written before any step 4 or stale-flag run). New scripts that
carry out registered parts: harness/lane3.sh runs step 4 (section 6: rm2.py series s4, arm g10, the step 2 items and CPUs) after the steps 2-3 driver ends; harness/mem_replay3.py and mem_all3.py are the 15 October harness's mem_replay.py and mem_all.py with only the paths changed (this run's folders, Graphicle 0.1.11's `graphicle`, the registered range files), mem_gold.py is copied unchanged (sha256 b2d4dd80..., as published); harness/probe_mem3.sh is a not-measured
probe of the 0.1.11 commands they call (the 15 October probe_mem.sh and probe_note.sh). sha256: lane3.sh f5ac3c5b...,
probe_mem3.sh 175338a6..., mem_replay3.py edad1060..., mem_all3.py 86012da4....

N3 (2026-11-01T13:53Z, note, not a method change; written before any stale-flag run). harness/lane4.sh (sha256
e7235060...) runs the stale-flag part after step 4: the probe, then mem_replay3.py and mem_all3.py per repository in
the published order (tokio, poetry, prometheus), one at a time on CPUs 36-39 (published: 92-95 and 88-91 in two sets), then summarize3.py into runs1/SUMMARY_MEM.json. It stops before the replays if the probe's note or re-anchor
command fails.

N4 (2026-11-01T15:24Z, note, analysis only). harness/final_counts.py (sha256 cc407fb5...) reads the finished result
files on the node (s2/TABLES.json, runs1/SUMMARY_MEM.json, memall results) and writes s2/FINAL_COUNTS.json: the
step 4 comparison counts and the stale-flag table values used in RESULTS.md. It ran after every measured run.
