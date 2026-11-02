# Benchmark results, datumesh-store 0.4.0: PostgreSQL and SQLite

Two runs of `datumesh-store bench` with the 0.4.0 tree, the invocation of 0.3.1 / 0.3.2:
`--cells 1000 --versions 100000 --dependents 1000 --samples 10000 --threads 6 --blob-max 0
--slice-cells 100000`, release build (`rust:1.99`), one after the other on the same host:
PostgreSQL on a new database `dm_bench_040` of the development server `datumesh-pg`
(postgres:16, default settings, over the Docker network; dropped afterwards), SQLite on a new
file on the container's local disk (removed afterwards). Loads: PostgreSQL
`synchronous_commit = off`, SQLite `synchronous = NORMAL`; the timed operations durable
(PostgreSQL's default; SQLite `synchronous = FULL`, the store's default). The output files
are verbatim (the SQLite load line says `synchronous_commit=off`: the label of that build;
fixed since to name both settings).

## p50 / p99 / mean (µs unless stated)

| Operation | PostgreSQL | SQLite |
|---|---|---|
| version_asof | 242 / 1308 / 291 | 277 / 817 / 312 |
| receipt | 168 / 1123 / 205 | 163 / 560 / 149 |
| put_version | 1768 / 6393 / 2032 | 556 / 3877 / 801 |
| dependents (1,000) | 1560 / 3462 / 1653 | 292 / 548 / 330 |
| computed_asof | 205 / 872 / 240 | 35 / 592 / 143 |
| computed_asof_with_receipt | 221 / 973 / 251 | 43 / 315 / 70 |
| stored_asof | 237 / 946 / 270 | 44 / 387 / 65 |
| stored_asof_with_receipt | 212 / 775 / 237 | 33 / 226 / 37 |
| put_computed | 1517 / 3356 / 1602 | 524 / 3177 / 753 |
| cell_dependents (1,000 cells) | 8049 / 14970 / 8576 | 8664 / 13953 / 9051 |
| resolve_binding | 178 / 500 / 194 | 7 / 21 / 9 |
| resolve_name | 162 / 458 / 177 | 8 / 22 / 9 |
| versions_between | 573 / 3352 / 742 | 420 / 1829 / 526 |
| slice_members bounded, 1,000 members | 91.6 / 162 / — ms | 123 / 259 / 133 ms |
| slice_members unbounded, 1,000 members | 60.3 / 94.2 / — ms | 87.6 / 118 / 91.1 ms |
| slice_members unbounded, 100,000 members | 7.29 / 7.50 s | 9.53 / 10.4 s |
| slice_members bounded, 100,000 members | 7.13 / 7.63 s | 10.2 / 10.9 s |
| slice_values bounded, 1,000 | 99.0 / 131 ms | 125 / 182 ms |
| slice_values unbounded, 1,000 | 73.9 / 128 ms | 96.5 / 150 ms |
| slice_values unbounded, 100,000 | 7.46 / 8.27 s | 10.4 / 12.1 s |
| slice_values bounded, 100,000 | 7.61 / 8.64 s | 11.1 / 12.8 s |
| slice_changed_after bounded, 1,000 | 25.7 / 83.6 ms | 86.9 / 188 ms |
| slice_changed_after unbounded, 1,000 | 18.4 / 38.0 ms | 69.7 / 106 ms |
| slice_changed_after unbounded, 100,000 | 2.00 / 2.57 s | 7.37 / 9.04 s |
| slice_changed_after bounded, 100,000 | 1.92 / 3.13 s | 8.23 / 9.60 s |
| put_manifest, 1,000 members | 73.6 / 114 ms | 162 / 251 ms |
| manifest, 1,000 members | 375 / 752 | 51 / 122 |
| put_manifest, 100,000 members | 7.71 / 9.92 s | 9.53 / 10.3 s |
| manifest, 100,000 members | 18.8 / 34.3 ms | 4.23 / 24.9 ms |

Loads: 100,000 versions in 168 s (PostgreSQL, 594/s) and 108 s (SQLite, 925/s); the
101,000 slice cells in 136 s and 225 s (SQLite: one writer at a time; verification runs in
parallel). Disk: PostgreSQL 2,103 MB, SQLite 1,564 MB.
