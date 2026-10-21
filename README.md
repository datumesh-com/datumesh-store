# datumesh-store

A PostgreSQL store for the [Datumesh](https://datumesh.com) protocol: function descriptors, signed receipts, blobs, versions of cells over transaction time, lineage, namespaces' key documents and function names. `PgStore` implements the `Store` trait and, through it, `datumesh-core`'s `Resolver`, so `datumesh_core::verify` recomputes receipts of stored cells from the database. It passes `datumesh-core`'s resolver test kit as a time-aware resolver.

- `datumesh-store`: the library.
- `datumesh-store-cli`: the `datumesh-store` command: `migrate [--check]`, `import`, `export`, `bench`.

## Example

```toml
[dependencies]
datumesh-store = "0.1.0"
datumesh-core = "0.3.1"
```

Store the key documents of `spec/vectors/receipt/stored-number.json`, its receipt (verified against them on the way in), its function's descriptor and the version it states, then verify the receipt through the store (it recomputes the stored value as of the receipt's `asof`):

```rust
use datumesh_core::json::{self, Value};
use datumesh_core::{verify, Keys, Level, Receipt, Resolver};
use datumesh_store::postgres::{Config, NoTls};
use datumesh_store::{migrate, PgStore, Store};

fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    // The schema; the `datumesh-store migrate` command does the same.
    let mut client = url.parse::<Config>().unwrap().connect(NoTls).unwrap();
    migrate::apply(&mut client).unwrap();
    let store = PgStore::connect(&url).unwrap();

    let vector = json::parse(&std::fs::read("stored-number.json").unwrap()).unwrap();
    let text = vector.get("input").and_then(Value::as_str).unwrap().as_bytes();
    let descriptor = vector.get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs();

    // Receipts are verified on the way in, against the key document stored for their
    // signer host: store the host's document first. The vector carries the documents.
    let mut keys = Keys::new();
    for (host, env) in vector.get("keys").and_then(Value::as_object).unwrap() {
        let body = env.get("https").and_then(|h| h.get("body")).and_then(Value::as_str).unwrap();
        store.put_namespace(host, body.as_bytes(), "2024-03-16T09:00:00.000000Z").unwrap();
        keys.insert_response(host, 200, Some(body.as_bytes()));
    }

    let fn_hash = store.put_descriptor(&descriptor).unwrap();
    let hash = store.put_receipt(text).unwrap();
    let receipt = Receipt::from_json(text).unwrap();
    let cell_id = receipt.cell().id();
    // Versions are append-only: on a second run the version is already there.
    if store.latest(&cell_id).unwrap().is_none() {
        store.put_version(&cell_id, receipt.tx(), &hash).unwrap();
    }

    let result = verify(text, &keys, Some(&store as &dyn Resolver), None);
    assert_eq!(result.level, Some(Level::Recomputed), "{:?}", result.error());
    println!("verified {} receipt {}", result.level.unwrap().as_str(), hash);
    println!("fx_rate is {fn_hash}; the cell's latest version is at {}", store.latest(&cell_id).unwrap().unwrap().tx);
}
```

It prints `verified recomputed receipt 19a5fe3a744c5792f747fb4c286ce396ab44b95922de4f07937502fe595b6f81`, then the function hash and the version's tx.

The API is synchronous. From async code, call it on a blocking thread (`tokio::task::spawn_blocking`). `PgStore` is `Send + Sync`; share one per process.

## The store

| Method | What it does |
|---|---|
| `put_descriptor(bytes)` | Checks the descriptor (`Descriptor::from_json`), stores its canonical form (JCS), returns the function hash. Idempotent. |
| `put_receipt(bytes)` | Verifies the receipt (SPEC 10.1 steps 1-6, `datumesh_core::verify`) against the key document stored for its signer host with `put_namespace`, so nothing unverified enters the store; refuses it with `verify`'s code otherwise (no document stored for the host: `E_KEY_UNAVAILABLE`). Stores its canonical form, records an edge to each input (held or not), returns the receipt hash. Idempotent. The same hash with other bytes is `Error::Conflict` and the stored receipt is kept: that is a second valid signature by the same key, which only a randomised signer makes. A rotated key does not cause it: the key id is in `computed_by`, which the hash covers, so a receipt signed with the new key has a new hash. |
| `put_blob(bytes)` / `blob(sha256)` | Content by SHA-256, in the database (`bytea`). |
| `put_version(cell_id, tx, receipt_hash)` | Appends: from `tx` on, the cell has the receipt's value. The receipt must be stored, of that cell, at that `tx`; `tx` must be later than the cell's latest version (`Error::NotLater`). |
| `version_asof`, `versions`, `latest` | The latest version with `tx <= asof`; the versions with `from <= tx <= to` in tx order; the latest. |
| `inputs`, `dependents` | Lineage edges, both ways. |
| `put_namespace(host, keydoc, validated_at)` / `namespace(host)` | A host's key document, checked as a 200 response for that host (`keys_from_response`), with its validation time; re-validation only moves forward. Store it before the host's receipts. A later document (a key revoked or dropped) does not touch receipts already stored: they verified when they were stored; it applies to every `put_receipt` from then on, including one repeating a stored receipt. |
| `bind_name(fn_name, fn_hash, tx)` / `resolve_name(fn_name, asof)` | Which function a fully qualified name meant from `tx` on. Append-only, in tx order; the descriptor must be stored. |
| `descriptor`, `receipt`, `evaluate` (`Resolver`) | Stored bytes (`Cow::Owned`); `evaluate` gives a value only for a `stored` function: that of `version_asof(cell, asof)`. |

Every `Store` method returns a `Result`. Malformed input is refused with the core's error code (`Error::Invalid`), without a query: hashes and cell ids are 64 lowercase hex digits, instants `YYYY-MM-DDTHH:MM:SS.ffffffZ`, hosts DNS names, function names fully qualified. A database failure is `Error::Database` (or `Error::PoolTimeout`); reads answer `Ok(None)` / `Ok(vec![])` only when nothing is stored. The `Resolver` methods (`descriptor`, `receipt`, `evaluate`) keep the core's `Option` signatures: a malformed argument or a database failure is `None` there (a failure is logged with `tracing`, target `datumesh_store`), which `verify` reports as unavailable.

`PgStore::connect(url)` connects without TLS; `PgStore::with_connector(f, PoolOptions)` takes any connection function (TLS included). The pool is the crate's own: a mutex and a condition variable, no global state, no background threads, a bounded number of connections, a checkout timeout, connections the server closed are replaced, and each statement is prepared once per connection. Every `put_*` is one transaction.

## Schema

Plain SQL in [`migrations/`](crates/datumesh-store/migrations) (`migrations` at the top is a link to it), embedded in the library, applied in order with a tracking table (`datumesh_schema_migrations`, with checksums) under an advisory lock.

- `descriptors(fn_hash, bytes)`, `receipts(hash, bytes, cell_id, tx, tx_at, fn_hash, publisher)`, `blobs(sha256, bytes, length, media_type)`, `versions(cell_id, tx, tx_at, receipt_hash)`, `edges(receipt_hash, input_hash, kind)` (`kind` is `receipt`; `slice` is allowed for later), `namespaces(host, keydoc, validated_at, validated_at_at, suspended_at, suspended_at_at)`, `name_bindings(fn_name, tx, tx_at, fn_hash)`.
- Hashes, ids, names and instants are `text COLLATE "C"`: instants (fixed width) compare chronologically as text, and every as-of query compares the text. Each instant is also `timestamptz` (`*_at`), checked equal to the text; it round-trips every instant from 0001 to 9999 exactly, whatever the session's TimeZone or DateStyle (tested).
- `versions` and `name_bindings` are append-only in the database: `BEFORE UPDATE OR DELETE` row triggers and `BEFORE TRUNCATE` statement triggers raise; a `BEFORE INSERT` trigger takes a per-cell (per-name) transaction-scoped advisory lock and refuses a `tx` not later than the latest, so concurrent writers, raw SQL included, are serialised. It requires READ COMMITTED (the store's sessions use it). A superuser can still disable triggers or edit the catalog; the triggers protect against mistakes and ordinary roles, not against the database owner.

`datumesh-store migrate --check` migrates a scratch database (the role needs `CREATEDB`), compares the two catalogs (relations, columns, constraints, indexes, triggers and their enabled state, functions, recorded migrations), prints the differences and exits 1 on drift; then it prepares every statement the store uses. The sync driver has no compile-time query checking, so the test suite prepares every statement against the migrated schema.

## The command

```sh
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres
datumesh-store migrate                 # apply; re-running is a no-op
datumesh-store migrate --check         # exit 1 on drift
datumesh-store import spec/vectors     # vector files or directories; JSON lines; - for stdin
datumesh-store export > store.jsonl    # JSON lines: descriptors, receipts, blobs, versions, namespaces, names
datumesh-store bench                   # see below
```

`export` writes one consistent snapshot in an order `import` replays (namespaces first: receipts are verified against them); importing what is already there changes nothing. Importing vectors stores each valid receipt vector's key documents (validated at the time of the import, unless the same document is already stored) before its receipts.

## Development

`docker compose up -d` starts PostgreSQL 16; with `DATABASE_URL` set, `cargo test --workspace` runs everything (each test in a database of its own, so the role needs `CREATEDB`). `tools/cargo.sh <toolchain> <args>` runs cargo in a `rust:<toolchain>` container next to a `datumesh-pg` container; `tools/package_test.sh` is the packaging test; `tools/bench.sh` the benchmark. The specification (`spec/`, a submodule at `spec-0.2.1`) provides the vectors.

## Benchmark

`tools/bench.sh` runs `datumesh-store bench` (release build) against a dedicated `postgres:16` container whose data directory is a host volume, then times a cold restart. Results on the development host (6 vCPU, 15 GB RAM, virtual disk; 2026-10-21), 1,000,000 versions over 10,000 cells (100 each), every version a receipt signed with feature `sign` and stored through `put_receipt` + `put_version` from 6 threads (546.9 s, 1,829 versions/s; loading sessions with `synchronous_commit = off`), plus 1,000 receipts depending on one input. Timings are one call at a time through `PgStore`, `synchronous_commit = on`, 10,000 samples:

| Operation | p50 | p99 | mean |
|---|---:|---:|---:|
| `version_asof` (random cell, random time) | 215 µs | 394 µs | 227 µs |
| `receipt` (random of the 1,000,000 version receipts) | 214 µs | 417 µs | 227 µs |
| `put_version` (new latest version; one transaction, fsync) | 2.06 ms | 3.43 ms | 2.12 ms |
| `dependents` (1,000 dependents) | 1.38 ms | 2.59 ms | 1.50 ms |
| after a restart with the page cache dropped (2,000 samples): `version_asof` / `receipt` | 605 / 900 µs | 1.21 / 1.50 ms | |

Disk: `receipts` 1,129 MB + 324 MB indexes, `versions` 197 MB + 314 MB indexes; 1,965 MB for all tables (about 2.0 KB per version, of which the canonical receipt is about 1.1 KB). Cold restart: `docker stop` 0.4 s (4.6 s right after the load, a shutdown checkpoint); after dropping the page cache, `docker start` to the first answered `version_asof` 1.2-1.4 s, of which about 0.5 s is starting the tool's own container; the probe itself answered 112-168 ms after it started.

Blobs (`put_blob` / `blob`, median): 1 MiB 33.8 / 4.9 ms, 4 MiB 127 / 19.6 ms, 16 MiB 330 / 63 ms, 64 MiB 1,683 / 298 ms; writes level off near 30-50 MB/s from 1 MiB on and every value is read and written whole, in memory on both sides. Keep blobs up to 1 MiB in the table; above that, object storage (content-addressed by the same SHA-256) is the better home.

Server settings (`tools/bench.sh`): `shared_buffers=4GB`, `effective_cache_size=11GB`, `work_mem=32MB`, `maintenance_work_mem=1GB`, `wal_buffers=16MB`, `max_wal_size=8GB`, `min_wal_size=1GB`, `checkpoint_completion_target=0.9`, `wal_compression=lz4`, `random_page_cost=1.1`, `effective_io_concurrency=200`, `max_worker_processes=6`, `max_parallel_workers=6`, `max_parallel_workers_per_gather=3`, `max_parallel_maintenance_workers=3`, `max_connections=50`, `jit=off`, `--shm-size=1g`; `fsync` and `synchronous_commit` on.

Licensed under Apache-2.0.
