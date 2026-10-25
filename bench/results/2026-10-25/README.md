# Benchmark results, datumesh-store 0.3.1

One run of `datumesh-store bench` with the 0.3.1 tree, for the slice operations it adds. The output file is verbatim.

| File | What it measures |
|---|---|
| `run-0.3.1-1000x100-slices100000.txt` | The 0.3.0 invocation of 2026-10-24 (1,000 cells × 100 versions, 1,000 dependents, 10,000 samples, 6 threads, no blobs) plus `--slice-cells 100000`: 100,000 `fx_rate` cells with quote `SLA` (one date each, from 2000-01-01 on) and 1,000 with quote `SLB`, one version each. |

## Invocation

The release build (`rust:1.99`, `datumesh-core` 0.3.4 from crates.io), against a new, empty database `dm_bench_031` on the development server `datumesh-pg` (dropped afterwards):

```sh
datumesh-store migrate
datumesh-store bench --cells 1000 --versions 100000 --dependents 1000 --samples 10000 --threads 6 --blob-max 0 --slice-cells 100000
```

The server is the development container (`postgres:16`, server 16.15) with its default settings (`shared_buffers` 128 MB, `work_mem` 4 MB, `random_page_cost` 4, `fsync` and `synchronous_commit` on), not the tuned server of `tools/bench.sh`. The database ends at 2.0 GB, mostly the 1.2 million `manifest_members` rows the `put_manifest` samples leave behind.

## The slice rows (p50 / p99)

| Operation | Shape | Samples | p50 | p99 |
|---|---|---|---|---|
| `slice_members` bounded | a range of 1,000 dates of the 100,000 `SLA` cells (base and quote bound) | 200 | 67.5 ms | 156.4 ms |
| `slice_members` unbounded | the 1,000 `SLB` cells (date open) | 200 | 53.2 ms | 85.1 ms |
| `slice_members` unbounded | the 100,000 `SLA` cells | 10 | 5.68 s | 6.36 s |
| `slice_members` bounded | a range over all 100,000 `SLA` dates | 10 | 5.67 s | 6.44 s |
| `versions_between` | a random range of a random cell's 100 versions, a random asof (22.1 versions returned on average) | 10,000 | 0.51 ms | 1.86 ms |
| `put_manifest` | 1,000 members (a new entry each time), one transaction | 200 | 62.1 ms | 89.7 ms |
| `put_manifest` | 100,000 members | 10 | 5.98 s | 7.01 s |
| `manifest` | 1,000 members (about 67 KB) | 4,000 | 0.28 ms | 0.93 ms |
| `manifest` | 100,000 members (about 6.7 MB) | 200 | 14.2 ms | 33.1 ms |

`slice_members` costs about 55-65 µs per member, linear in the members: per member, the candidate's `cells` row, a check of each constrained argument on the `cell_args` primary key, the version answering (backward scan of `versions_pkey`, then `receipts_pkey`), its key arguments, and about 0.9 KB of receipt bytes to the client, then a sort in the client. `EXPLAIN ANALYZE` of the bounded 1,000-member query shows an index-only range scan of `cell_args_value` for the candidates and index lookups for the rest (no sequential scan). The other rows are within run-to-run noise of 0.3.0's.

## Host

As on 2026-10-22: a QEMU/KVM VM, 6 vCPU, 15 GB RAM, virtual disk, Ubuntu 20.04, Docker. The TLS test server `datumesh-tls-pg` was running but idle; other containers were building and testing other crates during the run.
