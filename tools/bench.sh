#!/bin/sh
# The benchmark of the README: a dedicated postgres:16 container with its data directory on
# a host volume and settings for a 6-core / 15 GB machine, the release build of
# datumesh-store, `bench` with the full sizes, then a cold restart (container stop, page
# cache dropped, start, first version_asof answered).
#
# Usage: tools/bench.sh [DATA_DIR] [bench options...]
#   DATA_DIR defaults to /root/datumesh-libs/bench-pgdata (outside the repository).
# Needs Docker, root (to drop the page cache) and the `datumesh` Docker network.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
data="${1:-/root/datumesh-libs/bench-pgdata}"
[ $# -gt 0 ] && shift
name=datumesh-bench-pg
url="postgres://postgres:postgres@$name:5432/postgres"

docker network inspect datumesh >/dev/null 2>&1 || docker network create datumesh >/dev/null
mkdir -p "$data"
docker rm -f "$name" >/dev/null 2>&1 || true
# shared_buffers 25% of RAM, effective_cache_size ~75%; WAL sized for a bulk load; SSD
# costs; parallelism for 6 cores; JIT off (short OLTP queries).
docker run -d --name "$name" --network datumesh --shm-size=1g \
  -v "$data:/var/lib/postgresql/data" -e POSTGRES_PASSWORD=postgres postgres:16 \
  -c shared_buffers=4GB -c effective_cache_size=11GB -c work_mem=32MB -c maintenance_work_mem=1GB \
  -c wal_buffers=16MB -c max_wal_size=8GB -c min_wal_size=1GB -c checkpoint_completion_target=0.9 \
  -c wal_compression=lz4 -c random_page_cost=1.1 -c effective_io_concurrency=200 \
  -c max_worker_processes=6 -c max_parallel_workers=6 -c max_parallel_workers_per_gather=3 \
  -c max_parallel_maintenance_workers=3 -c max_connections=50 -c jit=off >/dev/null
until docker exec "$name" pg_isready -U postgres -q 2>/dev/null; do sleep 1; done
sleep 2

run() {
  docker run --rm --network datumesh -v "$HOME/.cache/datumesh-cargo:/usr/local/cargo/registry" -v "$root:/w" -w /w \
    -e CARGO_TARGET_DIR=/w/target-release -e DATABASE_URL="$url" rust:1.99 "$@"
}
run cargo build -q --release --locked -p datumesh-store-cli
bin=/w/target-release/release/datumesh-store
echo "== settings"
docker exec "$name" psql -U postgres -Atc "SELECT name || ' = ' || setting || coalesce(' ' || unit, '') FROM pg_settings WHERE name IN ('shared_buffers','effective_cache_size','work_mem','maintenance_work_mem','wal_buffers','max_wal_size','min_wal_size','checkpoint_completion_target','wal_compression','random_page_cost','effective_io_concurrency','max_worker_processes','max_parallel_workers','max_parallel_workers_per_gather','max_connections','jit','synchronous_commit','fsync','server_version') ORDER BY name"
echo "== migrate"
run $bin migrate
echo "== bench $*"
run $bin bench "$@"
echo "== cold restart"
t0=$(date +%s%N)
docker stop -t 60 "$name" >/dev/null
t1=$(date +%s%N)
sync
echo 3 > /proc/sys/vm/drop_caches
t2=$(date +%s%N)
docker start "$name" >/dev/null
run $bin bench --wait
t3=$(date +%s%N)
echo "stop: $(( (t1 - t0) / 1000000 )) ms; start -> first version_asof answered (incl. a fresh tool container): $(( (t3 - t2) / 1000000 )) ms"
echo "== after the restart (cold page cache), once more without loading"
run $bin bench --skip-load --samples 2000 --blob-max 0
echo "== data directory"
du -sh "$data"
