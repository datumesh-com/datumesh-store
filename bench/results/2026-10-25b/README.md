# Benchmark results, datumesh-store 0.3.2

One run of `datumesh-store bench` with the 0.3.2 tree, the invocation of 0.3.1 (`bench/results/2026-10-25`): `--cells 1000 --versions 100000 --dependents 1000 --samples 10000 --threads 6 --blob-max 0 --slice-cells 100000`, release build (`rust:1.99`), a new database `dm_bench_032` on the development server `datumesh-pg` (postgres:16, default settings; dropped afterwards). The output file is verbatim. The host was not quiet: other containers were building and testing during the run (the 0.3.1 rows in this run are 10-20 % slower than in the 0.3.1 run).

## The rows of 0.3.2 (p50 / p99 / mean), against `slice_members` in the same run

| Shape | `slice_members` | `slice_values` | `slice_changed_after` |
|---|---|---|---|
| bounded, 1,000 members (a range of 1,000 of 100,000 cells) | 78.2 / 165 / 82.9 ms | 87.2 / 145 / 90.8 ms | 24.9 / 59.9 / 24.3 ms |
| unbounded, 1,000 members (every cell of a quote) | 60.1 / 93.3 / 63.4 ms | 70.4 / 107 / 73.2 ms | 18.5 / 40.1 / 15.8 ms |
| unbounded, 100,000 members | 6.70 / 7.69 / 6.78 s | 7.10 / 8.10 / 7.19 s | 2.01 / 2.47 / 1.13 s |
| bounded, 100,000 members (a range over all) | 6.84 / 7.66 / 6.76 s | 7.01 / 7.72 / 7.05 s | 1.95 / 2.75 / 1.16 s |

`slice_changed_after` samples alternate: as of A after the load (unchanged: the answer is `false`, every candidate or recent receipt looked at) and as of before it (changed: the first match ends the query). Its means are the average of the two; the unchanged half is the p50 side (about 2 s at 100,000), the changed half a few hundred milliseconds at most.

- `slice_values` costs the same as `slice_members` in the database, about 70 µs per member here: the time is the per-member index lookups (the candidate's constraints, the version answering, its key and its arguments), not the receipt bytes (about 0.9 KB a member) it leaves out; it carries the member's cell instead (one more lookup of its arguments). What it saves is in the caller: no receipt is parsed (`Receipt::from_json`) to get a value or arguments.
- `slice_changed_after` for 1,000 members: 3-4 times cheaper than reading the members. At 100,000 members with nothing changed it still looks at every candidate (or every recent receipt of the function, whichever two bounded probes find fewer): in this data set the function's other 100,000 cells all have receipts after A, so neither side is small. It is cheap when A is recent (few receipts after it), the engine's case.
