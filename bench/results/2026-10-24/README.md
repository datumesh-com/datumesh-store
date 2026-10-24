# Benchmark results, datumesh-store 0.2.0 and 0.3.0

One run of `datumesh-store bench` at a smaller size than the 0.1.0 runs, for the operations 0.2.0 adds. The output file is verbatim.

| File | What it measures |
|---|---|
| `run-0.3.0-1000x100.txt` | The same invocation with the 0.3.0 tree (database `dm_bench_030`, dropped afterwards), which adds `resolve_binding` and `resolve_name`: 100 names × 100 bindings each (one an hour, alternating between two functions), a random name as of a random time within (and an hour either side of) its bindings. |
| `run-0.2.0-1000x100.txt` | 1,000 cells × 100 versions = 100,000 versions, 1,000 dependents (each a cell of its own), 10,000 samples, 6 loading threads, no blobs. Every version's receipt is also a computed version of its cell (put per cell newest first, as a historical backfill would), and so is every dependent. |

## Invocation

The release build of the 0.2.0 tree (`rust:1.99`), against a new, empty database `dm_bench_020` on the development server `datumesh-pg` (dropped afterwards):

```sh
datumesh-store migrate
datumesh-store bench --cells 1000 --versions 100000 --dependents 1000 --samples 10000 --threads 6 --blob-max 0
```

The server is the development container (`postgres:16`, server 16.15) with its default settings, not the tuned server of `tools/bench.sh`: `shared_buffers` 128 MB, `effective_cache_size` 4 GB, `work_mem` 4 MB, `random_page_cost` 4, `fsync` and `synchronous_commit` on. The whole database (313 MB) is smaller than the host's page cache, so these are warm-cache numbers; compare them with each other and with `version_asof` / `receipt` in the same file, not with the 1,000,000-version runs of 2026-10-22.

## What the rows show

- `computed_asof`, `stored_asof`: one indexed lookup each, the same cost as `version_asof` (p50 about 220-240 µs, mostly the round trip).
- `*+receipt` (the read, then `receipt` of the answer) against `*_with_receipt` (one query): 397 / 387 µs against 222 / 219 µs p50. One query saves the second round trip, about 45 %, which is why the `*_with_receipt` methods exist.
- `put_computed`: 1.72 ms p50, one transaction with fsync, a little under `put_version` (1.95 ms), which also takes a per-cell lock.
- `cell_dependents` with 1,000 dependent cells: 6.4 ms p50. The plan uses the existing indexes (`receipts_cell_tx` for the cell's receipts, `edges_input_hash`, `receipts_pkey` for each dependent's cell); the cost is the 1,000 lookups of the dependents' cell ids. A covering index `receipts (hash) INCLUDE (cell_id)` (index-only lookups) gained about 7 % in a trial and was not kept. Storing the dependent's cell id on each edge (a denormalised column) answered the same query in about 2.4 ms in a trial (psql, `\timing`), at the cost of a wider `edges` table; not done, since the contract derives the answer from edges joined to receipts.

- 0.3.0, `resolve_binding`: 188 / 464 µs p50 / p99 (mean 202), against `resolve_name` 169 / 362 µs in the same run: one lookup on the `name_bindings` primary key (fn_name, tx); the binding's `tx` is a second column of the same row, the difference mostly the extra text column decoded. The other rows of the 0.3.0 run are within run-to-run noise of 0.2.0's (no query changed except the `Resolver`'s `evaluate`, which `bench` does not time).

## Host

As on 2026-10-22: a QEMU/KVM VM, 6 vCPU, 15 GB RAM, virtual disk, Ubuntu 20.04, Docker. The TLS test server `datumesh-tls-pg` was running but idle.
