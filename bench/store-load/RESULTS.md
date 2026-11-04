# Store under load: SQLite 0.1.12 against PostgreSQL 16

These are the results of `run.py` for the change that moves the store to
PostgreSQL. Every number here comes from a `run.json` written by the
commands below. `table.py` prints the tables from those files.

## The machine

The test machine is smaller than the production host (12 GB, 6 cores):

- 2 vCPU and 8 GB of memory, with the benchmark's processes limited to
  5.8 GB by a memory cgroup.
- Linux 6.18. One virtio disk, about 500 MB/s direct.
- PostgreSQL 16 on the same machine, over its Unix socket:
  `shared_buffers = 512MB`, `max_wal_size = 4GB`, `min_wal_size = 512MB`,
  `synchronous_commit = on`, `max_connections = 120`.

On this machine the runs use 10 readers instead of the production host's 33.
33 readers on 2 vCPUs measure the CPU queue more than the store; the
33-reader runs are listed further down for reference.

**Throttled** runs put both Graphicle processes, and for PostgreSQL the
server's processes too, in a cgroup v1 `blkio` group limited to 320 MB/s
and 3,000 IOPS. cgroup v1 charges only direct and synchronous writes, not
buffered writeback, so the limit changes little for either engine.

## The corpus

- Seed: `kubernetes/pkg/kubelet`, 11,874 nodes and 39,255 edges. Readers
  query this project.
- Indexed under load: a 1,123-file Kubernetes subset (12,216 nodes, 43,342
  edges), registered with `register` and written by a separate `graphicle
  sync PATH --project ID --json` process, as the gate does.

## Commands

The SQLite runs, and the first PostgreSQL runs (before the fixes below):

```sh
# run4.sh: for each run, a fresh data folder and dropped caches, then
python3 -I bench/store-load/run.py --bin BIN --data-dir DATA \
  --seed ~/corpus/kubernetes/pkg/kubelet --index ~/corpus/k8s-index2 \
  --out runs/NAME --label NAME --clients 10 --warmup 60 --after 120 --sample 10 \
  [--cgroup /sys/fs/cgroup/blkio/gbench] [--psql "psql -h /tmp -U graphicle -d graphicle"]
```

`BIN` is `graphicle` 0.1.12 built from the starting commit (SQLite) or this
branch (PostgreSQL). The final PostgreSQL runs (`run_pg_final.sh`) use the
same arguments with this branch's final build. Each one first drops every
`g_%` schema and runs `CHECKPOINT`.

The no-reader runs use `--clients 0 --warmup 5 --after 5`. The phase
timings come from the same runs with `--no-strace --env
GRAPHICLE_PHASE_TIMINGS=1`.

## 10 readers

Each phase shows p50 / p99 latency of an MCP tool call and reads per
second. A failed read is one that returned an error.

| Run | Seed | Index | Reads before | Reads during indexing | Reads after | Peak RSS (MiB) | WAL | fsyncs |
|---|---|---|---|---|---|---|---|---|
| SQLite, local disk | 16.9 s | 58.0 s | 223 / 1320 ms, 32.2/s | 323 / 1833 ms, 21.1/s, 0 of 1227 failed | 246 / 1451 ms, 28.3/s | server 1352, indexer 156 | peak 131.5 MiB, end 0.0 MiB | server 26352, indexer 30 |
| PostgreSQL, first port, local disk | 22.6 s | 70.8 s | 294 / 1529 ms, 24.8/s | 349 / 6877 ms, 14.6/s, **3 of 1039 failed** | 307 / 1428 ms, 24.3/s | server 1370, indexer 161, PostgreSQL PSS 762 (RSS 2585) | pg_wal 704 MiB | server 1, indexer 4 |
| **PostgreSQL, final, local disk** | 20.8 s | 88.1 s | 264 / 1428 ms, 26.2/s | 346 / 1906 ms, 20.4/s, 0 of 1807 failed | 277 / 1554 ms, 24.9/s | server 1358, indexer 291, PostgreSQL PSS 523 (RSS 1834) | pg_wal 1104 MiB | server 1, indexer 4; PostgreSQL WAL syncs 12711 |
| SQLite, throttled | 15.4 s | 51.5 s | 228 / 1314 ms, 31.5/s | 330 / 1822 ms, 21.1/s, 0 of 1079 failed | 242 / 1388 ms, 28.6/s | server 1295, indexer 155 | peak 111.5 MiB, end 0.0 MiB | server 25683, indexer 32 |
| PostgreSQL, first port, throttled | 23.5 s | 63.8 s | 306 / 1449 ms, 24.3/s | 394 / 6993 ms, 14.0/s, **5 of 906 failed** | 298 / 1497 ms, 23.8/s | server 1297, indexer 396, PostgreSQL PSS 772 (RSS 2276) | pg_wal 704 MiB | server 1, indexer 4 |
| **PostgreSQL, final, throttled** | 21.3 s | 90.6 s | 276 / 1677 ms, 24.8/s | 353 / 1910 ms, 20.2/s, 0 of 1834 failed | 287 / 1669 ms, 23.3/s | server 1357, indexer 289, PostgreSQL PSS 524 (RSS 1773) | pg_wal 1104 MiB | server 1, indexer 4; PostgreSQL WAL syncs 12261 |

How to read the columns:

- **WAL.** For SQLite this is the `-wal` file. Its checkpoints worked here:
  it peaked near 130 MiB and returned to 0. For PostgreSQL it is the size of
  the `pg_wal` directory. That is pre-allocated and recycled segments,
  bounded by `max_wal_size` (4 GB here) whatever readers do; it is not data
  waiting for a checkpoint. During the final local run PostgreSQL wrote
  209 MB of WAL (`pg_stat_wal.wal_bytes`).
- **fsyncs.** These are the Graphicle processes' own fsyncs (strace). With
  PostgreSQL the server makes them; `wal_sync` counts them (12,711 in the
  final local run against SQLite's 26,352).
- **Memory.** PostgreSQL PSS counts its shared buffers once. Its RSS counts
  them once per backend that has touched them.

## Without readers

| Run | Seed | Index | Peak RSS (MiB) |
|---|---|---|---|
| SQLite | 15.6 s | 17.9 s | server 494, indexer 158 |
| PostgreSQL, first port | 24.2 s | 39.8 s | server 480, indexer 280 |

The phase timings (ms, summed over the sync and the server's projection job):

| Phase | SQLite | PostgreSQL, first port | PostgreSQL, final |
|---|---|---|---|
| Index, end to end | 16.3 s | 29.4 s | 22.9 s |
| `sync_commit` | 6,088 | 8,183 | 7,303 |
| `sync_passage_index` | 2,872 | 4,514 | 3,865 |
| `graph_extract` | 6,161 | 7,048 | 6,671 |
| `graph_persist` | 988 | 5,848 | 2,938 |
| `job_link` | under 300 | 5,427 | 3,444 |

## What the runs found, and what changed

1. **Reads waited for the write lock.** The first port failed 3 to 5 reads
   per run with `database is locked`. PostgreSQL's lock-wait log and new
   wait logging (`waited N s for the write lock at FILE:LINE`) showed two
   causes:
   - Every HTTP request took the store's write lock twice, to count itself
     against the workspace's request budget (`product.rs`).
   - Every MCP message took it to check its project exists (`api.rs`).
   Indexing holds the lock for up to 5 s per transaction, so reader
   requests queued behind it. Some waited past the 10 s lock timeout.

   Now the project check reads a snapshot. The request counters lock only
   their own row (`Store::counter`); the budget check still sees every
   request before it. The `graph_as_of`, episode and cross-project edge
   tools also moved to snapshots. Result: 0 failures, and a p99 during
   indexing of 1.9 s, the same as SQLite's 1.8 s.

2. **Indexing was slower on PostgreSQL**, because row-at-a-time statements
   each make a round trip. Batched now: cross-project edges and unresolved
   imports, symbol heads, unresolved calls, passage terms and files, and the
   graph's dependency rows.

   The dependency-row delete first went from 5.8 s to 39 s. Without table
   statistics (a store's first projection), PostgreSQL planned
   `dependent_id = ANY(array)` as a filter over every row of the workspace.
   It now calls `graphicle_forget_dependents`, one primary-key lookup per id
   in one round trip.

   Indexing alone now takes 22.9 s against SQLite's 16.3 s. Under 10
   readers it takes 88 to 91 s against SQLite's 52 to 58 s. Readers no
   longer stall behind the lock, so during indexing they make 40% more
   calls per second than in the first port (20.4/s against 14.6/s). That
   leaves the indexer less of the 2 vCPUs.

## 33 readers (first port only)

These ran before the fixes above, with 33 readers on 2 vCPUs. They are kept
because 33 is the production count. They show the same lock waits, at 33
readers instead of 10.

| Run | Seed | Index | Reads before | Reads during indexing | Reads after | Peak RSS (MiB) | WAL | fsyncs |
|---|---|---|---|---|---|---|---|---|
| SQLite, local disk | 15.6 s | 76.4 s | 805 / 5102 ms, 26.8/s | 1023 / 6468 ms, 20.9/s, 0 of 1659 failed | 775 / 6688 ms, 23.9/s | server 1781, indexer 148 | peak 123.4 MiB, end 0.0 MiB | server 25432, indexer 30 |
| PostgreSQL, first port, local disk | 56.5 s | 100.8 s | 1272 / 2964 ms, 23.4/s | 1578 / 11103 ms, 11.0/s, **101 of 1132 failed** | 1290 / 2587 ms, 23.4/s | server 1742, indexer 224, PostgreSQL PSS 605 (RSS 2264) | pg_wal 1024 MiB | server 1, indexer 4 |

## What this does not show

The 8.3 GB WAL of the outage needs readers that never leave a gap, for
hours. SQLite's checkpoints found gaps in every run here. `wal_starvation.py`
reproduces the starvation directly: one reader always in flight, and the
WAL grows by everything written.

PostgreSQL removes that failure mode by design: its WAL is recycled at
checkpoints, whatever readers are doing. These runs do not prove that at
production scale. Run them on the production host at 33 readers, with the
production repository, before relying on them for capacity.

## Recommendation

Use PostgreSQL, as this change does. It ends the outage's failure mode,
since the WAL is bounded by `max_wal_size`. With the fixes above, reads
during indexing are as fast and as reliable as on SQLite (0 failures, p99
1.9 s against 1.8 s).

The cost is indexing speed: about 1.4 times SQLite's time alone and 1.6
times under load, on this 2-vCPU machine. There is also a separate server
to run and back up. On the production host's 6 cores the CPU contention
behind most of the gap under load is smaller. Measure it there before
sizing the gate's limits.
