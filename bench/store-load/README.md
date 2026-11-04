# Store under load

`run.py` runs the indexing flow from the 2026-11-03 outage on one machine.
A server answers MCP reads while a separate `graphicle sync` process writes
a large repository into the same store. It records read latency, failed
reads, memory, store and WAL size, and fsyncs, before, during and after
indexing. It uses only the Python standard library. It runs the same against
either store engine: the `graphicle` you pass decides which one is used.

`wal_starvation.py` reproduces the outage's cause on SQLite alone: with a
reader always in flight, a PASSIVE autocheckpoint never resets the WAL, so
the WAL grows by everything written. It is kept for SQLite stores (0.1.12
and earlier) and is not needed for PostgreSQL ones.

## What a run does

1. Empties `--data-dir` and syncs `--seed` into it, so readers have a large
   code graph to query.
2. Starts `graphicle serve` on loopback, under `strace -f -c` unless
   `--no-strace` is given.
3. Starts `--clients` readers. Each calls the seed project's HTTP MCP
   endpoint in a loop with the weighted tool mix in `MIX` (the server's own
   tool names). Each reader opens a new MCP session every
   `--calls-per-pass` calls.
4. After `--warmup` seconds, registers an empty project over the API and
   runs `graphicle --data-dir D sync INDEX --project ID --json` as its own
   process, as the gate does.
5. Keeps reading for `--after` seconds once the sync exits, then stops.

The output in `--out` is `run.json` (the summary), `samples.csv` (every
`--sample` seconds), `calls.csv` (every call) and each process's log.
`run.json` holds, for each phase (warmup, indexing, after): the p50 and p99
latency, reads per second and failed reads, with the first error per tool.
It also records the seed and index times, peak RSS of the server and the
indexer, and their fsyncs (from strace).

With `--psql "psql -h HOST -U USER -d DB"` the run also records the
PostgreSQL side: the store schema's size, `pg_wal` size, the server
processes' RSS and PSS (PSS counts shared buffers once), and the WAL
records, bytes, writes and syncs from `pg_stat_wal` during the run.

## Running it

```sh
python3 -I bench/store-load/run.py \
  --bin target/release/graphicle \
  --data-dir /tmp/load-data \
  --seed ~/src/kubernetes/pkg/kubelet \
  --index ~/src/k8s-index \
  --out /tmp/load-run --label postgres-local \
  --clients 10 --warmup 60 --after 120 --sample 10 \
  --psql "psql -h /var/run/postgresql -d graphicle"
```

Pick `--clients` to fit the host. The production host (12 GB, 6 cores)
serves 33 concurrent readers, the default. On a smaller test machine, 10
readers is enough to see lock waits and WAL growth.
[RESULTS.md](RESULTS.md) has the results for the move to PostgreSQL: 10
readers on a 2-vCPU machine with a 5.8 GB memory limit. `table.py RUN_DIR…`
prints a run's `run.json` as a row of those tables.

For a slower disk, put both processes in a block-I/O cgroup with
`--cgroup DIR`. With cgroup v1 `blkio`, only direct and synchronous writes
are charged, not buffered writeback, so the limit makes little difference
to either engine. To throttle PostgreSQL too, move its processes into the
same cgroup (`echo PID > DIR/cgroup.procs` for each `postgres` process).

Set `GRAPHICLE_DATABASE_URL` (and `--env` for anything else the processes
need) before running against PostgreSQL. Each data folder gets its own
schema. To drop the schemas a run left, use
`DROP SCHEMA g_… CASCADE` for each `g_%` schema.

## Reading the results

- Failed reads during indexing are the main result. On SQLite they are
  `database is locked`. On PostgreSQL the same text is returned for a lock
  wait that passed the busy timeout (`lock_timeout`, 10 s).
- The WAL on SQLite should return to 0 once indexing ends. If it grows by
  the size of everything written, checkpoints are being starved, as in the
  outage. PostgreSQL's `pg_wal` is bounded by `max_wal_size` and recycled
  by its own checkpoints.
- Memory figures are RSS. For PostgreSQL, PSS is the fairer total, because
  each backend's RSS counts the shared buffers it has touched.
