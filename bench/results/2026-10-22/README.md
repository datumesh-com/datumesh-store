# Benchmark results, datumesh-store 0.1.0

Two full runs of `tools/bench.sh` on the development host, run on 2026-10-21 (Europe time) and committed on 2026-10-22. The output files are verbatim (the `exit` line was appended by the invocation below). Run 2 gives the numbers in the top-level README and in the 0.1.0 release.

| File | Code | What it measures |
|---|---|---|
| `run-2-verify-on-put.txt` | `92eaa26` (the bench and store code are identical at `v0.1.0`, except that `63d6eb5` changed only how a receipt that is already stored is put again) | `put_receipt` verifies signatures. Load 660.2 s, 1,515 versions/s. |
| `run-1-before-verify-on-put.txt` | `7d0b074` | Before `put_receipt` verified signatures. Load 546.9 s, 1,829 versions/s. The final post-restart step failed (`db error`, exit 1), a bug in `bench --skip-load` that `947093c` fixed. Every earlier section is complete. |

## Invocation

From the repository root, with the `datumesh` Docker network present, as root (the script drops the page cache), with each data directory new and empty:

```sh
tools/bench.sh /root/datumesh-libs/bench-pgdata   > bench-full.txt   2>&1; echo exit $? >> bench-full.txt     # run 1
tools/bench.sh /root/datumesh-libs/bench-pgdata-2 > bench-full-2.txt 2>&1; echo exit $? >> bench-full-2.txt   # run 2
```

No bench options were passed, so the defaults applied: 10,000 cells × 100 versions = 1,000,000 versions, 1,000 dependents, 10,000 samples, 6 threads, blobs up to 64 MiB. `tools/bench.sh` itself sets the PostgreSQL settings and records them in each file's `== settings` section. It builds `datumesh-store-cli` in release mode in `rust:1.99` and runs it against a dedicated `postgres:16` container (`datumesh-bench-pg`, server 16.15) whose data directory is the host volume given as the first argument. The data directories (4.7 GB and 4.4 GB) were deleted after this commit.

To reproduce at the same code: `git checkout 92eaa26 && git submodule update`, then the run-2 line above with a new data directory.

## Host

- VM under QEMU/KVM: 6 vCPU (Intel Core Processor, Broadwell, no TSX, IBRS), 15 GB RAM.
- Disk: a 400 GB QEMU virtual disk (`QEMU_HARDDISK`, reported as rotational), ext4.
- Ubuntu 20.04, kernel 5.4.0-105-generic, Docker 28.1.1.
- Nothing else ran a load on the host during the runs. The development database `datumesh-pg` was running but idle.
