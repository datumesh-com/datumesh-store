# datumesh-store

A PostgreSQL store for the [Datumesh](https://datumesh.com) protocol: function descriptors, signed receipts, blobs, versions of cells over transaction time (stored and computed), lineage (slices and their manifests included), the members of slices, namespaces' key documents and function names. `PgStore` implements the `Store` trait and, through it, `datumesh-core`'s `Resolver`, so `datumesh_core::verify` recomputes receipts of stored cells from the database. It passes `datumesh-core`'s resolver test kit as a time-aware resolver under the kit's default answer rule (`AnswerRule::TxAndAsof`: a version answers as of A when its `tx` and its receipt's `asof` are both at or before A), with the slice checks (`Kit::slices(true)`: it serves manifests). `Store` is sealed: only this crate implements it (see [The store](#the-store)).

- `datumesh-store`: the library.
- `datumesh-store-cli`: the `datumesh-store` command: `migrate [--check]`, `import`, `export`, `bench`.

## Example

```toml
[dependencies]
datumesh-store = "0.3.0"
datumesh-core = "0.3.3"
```

Store the key documents of `spec/vectors/receipt/stored-number.json`, its receipt (verified against them on the way in), its function's descriptor and the version it states, then verify the receipt through the store (it recomputes the stored value as of the receipt's `asof`):

```rust
use datumesh_core::json::{self, Value};
use datumesh_core::{verify, Keys, Level, Receipt, Resolver};
use datumesh_store::{migrate, PgStore, Store};

fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    // A store connects to an empty database too; the schema comes through it (the
    // `datumesh-store migrate` command does the same). Re-running is a no-op.
    let store = PgStore::connect(&url).unwrap();
    migrate::apply(&store).unwrap();

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
| `put_descriptor(bytes)` | Checks the descriptor (`FunctionDescriptor::from_json`: spec-0.4.0's, so `signature.order` and argument `extent` are accepted since 0.3.1; formula descriptors always carry `order`), stores its canonical form (JCS), returns the function hash. Idempotent. |
| `put_receipt(bytes)` | Verifies the receipt (SPEC 10.1 steps 1-6, `datumesh_core::verify`) against the key document stored for its signer host with `put_namespace`, so nothing unverified enters the store; refuses it with `verify`'s code otherwise (no document stored for the host: `E_KEY_UNAVAILABLE`). Stores its canonical form, records an edge to each input (held or not; a slice entry `slice:<slice id>:<members hash>` as a slice edge, since 0.3.1, without needing its manifest), indexes its cell by its typed arguments, returns the receipt hash. Idempotent: bytes whose canonical form is the stored copy return its hash without checking the signature again (it verified when stored), even after its key is revoked. Other bytes for a stored hash are verified first; if they verify, they are `Error::Conflict` and the stored receipt is kept: that is a second valid signature by the same key, which only a randomised signer makes. A rotated key does not cause it: the key id is in `computed_by`, which the hash covers, so a receipt signed with the new key has a new hash. |
| `put_blob(bytes)` / `blob(sha256)` | Content by SHA-256, in the database (`bytea`). |
| `put_version(cell_id, tx, receipt_hash)` | Appends: from `tx` on, the cell has the receipt's value. The receipt must be stored, of that cell, at that `tx`; `tx` must be later than the cell's latest version (`Error::NotLater`). |
| `version_asof`, `versions`, `latest` | The latest version with `tx <= asof`; the versions with `from <= tx <= to` in tx order; the latest. A `Version` carries its receipt's `asof` too. |
| `stored_asof(cell_id, asof)` | The stored version that answers as of A: the latest (by tx) with `tx <= A` whose receipt's `asof` is also `<= A`. A late-signed receipt (asof later than tx) answers only from its asof on; `version_asof` looks at tx only. |
| `put_computed(cell_id, tx, receipt_hash)` | Records a computed version: derived, not a fact, so insert-only in any order (a historical backfill adds an older version after a newer one), keyed on (cell_id, tx, asof) with the receipt's asof. The receipt is checked as for `put_version`. Idempotent; another receipt at an existing key is `Error::Conflict`. |
| `computed_asof(cell_id, asof)` | The computed version with the greatest `asof <= A` (ties: the greatest tx). |
| `stored_asof_with_receipt`, `computed_asof_with_receipt` | The same answers with the receipt's bytes, in one query. |
| `inputs`, `dependents` | Lineage edges to receipt hashes, both ways (slice entries are not among them). |
| `slice_entries(receipt_hash)`, `slice_dependents(entry)` | Since 0.3.1: the slice entries among a stored receipt's inputs; the stored receipts citing an entry. |
| `cell_dependents(cell_id)` | The cells that read a cell: those of stored receipts listing any stored receipt of the cell among their inputs, or (since 0.3.1) a slice entry whose stored manifest lists one. Ascending, unique. |
| `put_manifest(bytes)` / `manifest(entry)` (`Resolver`) / `member_manifests(receipt_hash)` | Since 0.3.1: a slice manifest (SPEC 15.13), checked by the core (`Manifest::from_json`), stored canonical under its entry, which `put_manifest` returns; idempotent, insert-only; its members need not be stored. `member_manifests` gives the entries of the stored manifests listing a receipt, so the readers of a slice are found from a member. |
| `slice_members(slice, asof)` | Since 0.3.1: the members of a slice (`datumesh_core::Slice`) as of A, each a `SliceMember { version, receipt }`: the cells of the slice's function (hash, `ns`, `fn`, the same argument names) whose bound arguments are equal, ranged ones within the range in the type's order (numbers as exact decimals, dates and instants chronologically, strings by code point) and open ones anything, with a stored version answering as of A (as `stored_asof`), in SPEC 15.13 key order (open and ranged arguments by ascending name, each in its type's order). A version slice is `versions_between` of its cell. A range scan of the typed-argument index. |
| `versions_between(cell_id, from, to, asof)` | Since 0.3.1: the stored versions with `from <= tx <= to` answering as of A, ascending, with their receipts: a version slice. Empty when `from > to`. |
| `put_namespace(host, keydoc, validated_at)` / `namespace(host)` | A host's key document, checked as a 200 response for that host (`keys_from_response`), with its validation time; re-validation only moves forward. Store it before the host's receipts. A later document (a key revoked or dropped) does not touch receipts already stored: they verified when they were stored, and putting one again stays idempotent; it applies to every new receipt from then on. |
| `bind_name(fn_name, fn_hash, tx)` / `resolve_name(fn_name, asof)` | Which function a fully qualified name meant from `tx` on. Append-only, in tx order; the descriptor must be stored. |
| `resolve_binding(fn_name, asof)` | The binding answering as of A, a `Binding { fn_hash, tx }`: the function hash and when the binding began (its `tx`). A name bound again to a hash it meant before begins again at the later `tx`. `resolve_name` is its `fn_hash`. |
| `descriptor`, `receipt`, `evaluate` (`Resolver`) | Stored bytes (`Cow::Owned`); `evaluate` gives a value only for a `stored` function: that of `stored_asof(cell, asof)` (tx and receipt asof both `<= asof`; until 0.2.0 it was `version_asof`'s, tx only). |

Every `Store` method returns a `Result`. Malformed input is refused with the core's error code (`Error::Invalid`), without a query: hashes and cell ids are 64 lowercase hex digits, instants `YYYY-MM-DDTHH:MM:SS.ffffffZ`, hosts DNS names, function names fully qualified. A database failure is `Error::Database` (or `Error::PoolTimeout`), holding a `DatabaseError`: `message()` (the server's message, or the driver's text), `code()` (the SQLSTATE, e.g. `42P01`), and the driver's error as its `source()`; reads answer `Ok(None)` / `Ok(vec![])` only when nothing is stored. The `Resolver` methods (`descriptor`, `receipt`, `manifest`, `evaluate`) keep the core's `Option` signatures: a malformed argument or a database failure is `None` there (a failure is logged with `tracing`, target `datumesh_store`), which `verify` reports as unavailable.

`Store` is sealed (a private supertrait): only this crate implements it. The store is the trust boundary: every receipt in it verified against its host's key document when it was stored, and callers rely on what it answers as checked; an implementation elsewhere could answer anything. It is still a trait object (`Arc<dyn Store + Send + Sync>`).

`PgStore::connect(url)` and `PgStore::connect_with(url, PoolOptions)` connect (TLS: see below). Connecting opens one connection and does nothing else on it (no statement is prepared, the schema is not looked at), so a store on an empty database is how the schema is made: `migrate::apply(&store)`. The pool is the crate's own: a mutex and a condition variable, no global state, no background threads, a bounded number of connections, a checkout timeout, connections the server closed are replaced, and each statement is prepared once per connection. Every `put_*` is one transaction.

## TLS

`connect` and `connect_with` always use a rustls connector (`tokio-postgres-rustls`, the ring provider chosen for that connector; no process-wide crypto provider is installed). Whether TLS is used is the URL's `sslmode`: `disable` never, `prefer` (the default) when the server offers it, `require` always. Whenever TLS is used, the server's certificate and host name are verified, against the Mozilla roots (`webpki-roots`, compiled in) plus any roots added through `PoolOptions`; there is no option to accept any certificate.

```rust
let pem = std::fs::read("ca.pem")?; // one or more certificates
let options = PoolOptions::new().tls_root_certificates_pem(&pem)?; // appended to the Mozilla roots
let store = PgStore::connect_with("postgres://app@db.internal/datumesh?sslmode=require", options)?;
```

`PoolOptions::tls_webpki_roots(false)` trusts only the added roots (a private CA). A bundle with no certificate, or one that does not parse, is `Error::Config`. A certificate that does not verify is `Error::Database` from `connect`, its message naming the reason (`invalid peer certificate: UnknownIssuer`, `certificate not valid for name ...`).

So a server with `ssl=on` and a self-signed certificate (or one from a private CA) fails even under the default `sslmode=prefer`: `prefer` does not fall back to plain text when the certificate does not verify. Add its CA with `tls_root_certificates_pem`, or connect with `sslmode=disable`. Client certificates are not configurable through `PoolOptions`; use `pg_unstable::with_connector` for them.

### `pg_unstable`

No type of the `postgres` driver or of a TLS crate is part of the API, except in the module `pg_unstable`, which is **outside semver until 1.0: it exposes the `postgres` driver's types, and any release may change it, including a patch release.** It holds `with_connector(f, PoolOptions)` (a store whose connections come from any function returning a `postgres::Client`: client certificates, another TLS setup, session settings), `Connector` (plain `postgres` clients that connect exactly as the store does) and `pub use postgres` (the driver this crate is built with). Depend on an exact version (`datumesh-store = "=0.3.1"`) if you use it.

## Schema

Plain SQL in [`migrations/`](crates/datumesh-store/migrations) (`migrations` at the top is a link to it), embedded in the library, applied in order with a tracking table (`datumesh_schema_migrations`, with checksums) under an advisory lock.

- `descriptors(fn_hash, bytes)`, `receipts(hash, bytes, cell_id, tx, tx_at, fn_hash, publisher, asof, asof_at)`, `blobs(sha256, bytes, length, media_type)`, `versions(cell_id, tx, tx_at, receipt_hash)`, `computed_versions(cell_id, tx, tx_at, asof, asof_at, receipt_hash)` (primary key `(cell_id, tx, asof)`), `edges(receipt_hash, input_hash, kind)` (`kind` is `receipt`), `namespaces(host, keydoc, validated_at, validated_at_at, suspended_at, suspended_at_at)`, `name_bindings(fn_name, tx, tx_at, fn_hash)`.
- `receipts.asof` (migration 2) is the receipt's own `asof`: backfilled from the stored canonical bytes when the migration runs, checked equal to them (a `CHECK`), and taken from them by a trigger when an insert does not name it (a 0.1.x writer).
- Migration 3 (0.3.1): `cells(cell_id, fn_hash, ns, fn, nargs)` and `cell_args(cell_id, name, type, value, num, fn_hash)`, the typed-argument index: every cell of a stored receipt, each argument's type and lexical form and, for a number, its exact `numeric` value, indexed by (function, argument, value) and (function, argument, number). A trigger on `receipts` derives the rows from the canonical bytes (so a 0.3.0 writer's receipts are indexed too), and the migration backfills them the same way. `manifests(entry, slice_id, members_hash, fn_hash, members, bytes)`, `manifest_members(entry, member_hash)` (indexed by member), `slice_edges(receipt_hash, entry)` (indexed by entry). All five are insert-only.
- Hashes, ids, names and instants are `text COLLATE "C"`: instants (fixed width) compare chronologically as text, and every as-of query compares the text. Each instant is also `timestamptz` (`*_at`), checked equal to the text; it round-trips every instant from 0001 to 9999 exactly, whatever the session's TimeZone or DateStyle (tested).
- `computed_versions` is insert-only in the database (`BEFORE UPDATE OR DELETE` and `BEFORE TRUNCATE` triggers raise) but in any order: there is no tx-order rule; its primary key serialises concurrent writers of one key.
- `versions` and `name_bindings` are append-only in the database: `BEFORE UPDATE OR DELETE` row triggers and `BEFORE TRUNCATE` statement triggers raise; a `BEFORE INSERT` trigger takes a per-cell (per-name) transaction-scoped advisory lock and refuses a `tx` not later than the latest, so concurrent writers, raw SQL included, are serialised. It requires READ COMMITTED (the store's sessions use it). A superuser can still disable triggers or edit the catalog; the triggers protect against mistakes and ordinary roles, not against the database owner.

`datumesh-store migrate --check` migrates a scratch database (the role needs `CREATEDB`; `migrate::check(&live, &scratch)` takes a store on each), compares the two catalogs (relations, columns, constraints, indexes, triggers and their enabled state, functions, recorded migrations), prints the differences and exits 1 on drift; then it prepares every statement the store uses. The sync driver has no compile-time query checking, so the test suite prepares every statement against the migrated schema.

## The command

```sh
export DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres   # ?sslmode=require for TLS only
export DATABASE_CA_FILE=ca.pem         # optional: roots to trust besides the Mozilla roots
datumesh-store migrate                 # apply; re-running is a no-op
datumesh-store migrate --check         # exit 1 on drift
datumesh-store import spec/vectors     # vector files or directories; JSON lines; - for stdin
datumesh-store export > store.jsonl    # JSON lines: namespaces, descriptors, receipts, manifests, blobs, versions, computed versions, names
datumesh-store bench                   # see below
```

Every command connects as `PgStore::connect_with` does (TLS as the URL's `sslmode` says, verified against the Mozilla roots plus `DATABASE_CA_FILE`). `export` writes one consistent snapshot in an order `import` replays (namespaces first: receipts are verified against them); importing what is already there changes nothing; exports of 0.1.x (without computed versions), 0.2.x and 0.3.0 (without manifests) import too. A store keeps one key document per host, the last validated, and an export carries only that one: receipts verified under a document that was replaced since cannot be imported from the export again (`E_KEY_UNKNOWN`). The specification's vectors are such a set since spec-0.4.0 (its slice-lineage receipt vectors sign for three hosts with other keys than the earlier vectors). Importing vectors stores each valid receipt vector's key documents (validated at the time of the import, unless the same document is already stored) before its receipts, with its recompute descriptor, inputs and (since 0.3.1) manifests, and each valid slice vector that is a manifest.

## Development

`docker compose up -d` starts PostgreSQL 16; with `DATABASE_URL` set, `cargo test --workspace` runs everything (each test in a database of its own, so the role needs `CREATEDB`). `tools/cargo.sh <toolchain> <args>` runs cargo in a `rust:<toolchain>` container next to a `datumesh-pg` container; `tools/package_test.sh` is the packaging test; `tools/bench.sh` the benchmark. The TLS tests need a server with TLS: `tools/tls_pg.sh` makes a throwaway CA and server certificate (in `target-tls/`, never committed) and starts the container `datumesh-tls-pg`; then set `DATABASE_TLS_URL=postgres://postgres:postgres@datumesh-tls-pg:5432/postgres` and `DATABASE_TLS_CA_FILE=/w/target-tls/ca.crt` (without them the TLS tests are skipped; CI sets `DATUMESH_REQUIRE_TLS_TESTS`, which makes them fail instead). `tools/public_api.sh` regenerates `public-api.txt` (`cargo public-api`) and checks that no driver or TLS type appears outside `pg_unstable`. The specification (`spec/`, a submodule at `spec-0.4.0`) provides the vectors.

## Benchmark

`tools/bench.sh` runs `datumesh-store bench` (release build) against a dedicated `postgres:16` container whose data directory is a host volume, then times a cold restart. Results on the development host (6 vCPU, 15 GB RAM, virtual disk; 2026-10-21), 1,000,000 versions over 10,000 cells (100 each), every version a receipt signed with feature `sign`, verified by `put_receipt` against the stored key document and stored with `put_version`, from 6 threads: 660.2 s, 1,515 versions/s (loading sessions with `synchronous_commit = off`; before `put_receipt` verified signatures the same load took 546.9 s, 1,829 versions/s). Plus 1,000 receipts depending on one input. Timings are one call at a time through `PgStore`, `synchronous_commit = on`, 10,000 samples:

| Operation | p50 | p99 | mean |
|---|---:|---:|---:|
| `version_asof` (random cell, random time) | 212 µs | 420 µs | 230 µs |
| `receipt` (random of the 1,000,000 version receipts) | 193 µs | 393 µs | 205 µs |
| `put_version` (new latest version; one transaction, fsync) | 1.90 ms | 3.88 ms | 2.04 ms |
| `dependents` (1,000 dependents) | 1.38 ms | 2.45 ms | 1.47 ms |
| after a restart with the page cache dropped (2,000 samples): `version_asof` / `receipt` | 536 / 915 µs | 1.23 / 1.60 ms | |

Disk: `receipts` 1,129 MB + 324 MB indexes, `versions` 197 MB + 314 MB indexes; 1,965 MB for all tables (about 2.0 KB per version, of which the canonical receipt is about 1.1 KB). Cold restart: `docker stop` 0.4 s (about 4 s right after the load, a shutdown checkpoint); after dropping the page cache, `docker start` to the first answered `version_asof` 1.2-1.6 s, of which about 0.5 s is starting the tool's own container; the probe itself answered 112-168 ms after it started.

Version 0.2.0's additions, at 1,000 cells × 100 versions on the development server with default settings (warm cache; `bench/results/2026-10-24`), p50 / p99: `computed_asof` 221 / 505 µs, `stored_asof` 226 / 476 µs (`version_asof` in the same run 238 / 534 µs), `computed_asof_with_receipt` 222 / 513 µs and `stored_asof_with_receipt` 219 / 516 µs (against 397 / 744 and 387 / 727 µs for the read followed by `receipt`), `put_computed` 1.72 / 2.69 ms, `cell_dependents` with 1,000 dependent cells 6.38 / 11.29 ms. Version 0.3.0's `resolve_binding` in the same setting: 188 / 464 µs (`resolve_name` 169 / 362 µs), 100 names × 100 bindings.

Version 0.3.1's slice operations in the same setting (`bench/results/2026-10-25`, `--slice-cells 100000`), p50 / p99: `slice_members` of 1,000 members 67.5 / 156 ms (a range of 1,000 of 100,000 cells) and 53.2 / 85.1 ms (every cell of a function with 1,000), of 100,000 members 5.68 / 6.36 s (about 57 µs per member, receipts included); `versions_between` 0.51 / 1.86 ms; `put_manifest` of 1,000 members 62.1 / 89.7 ms, of 100,000 5.98 / 7.01 s; `manifest` of 1,000 members 0.28 / 0.93 ms, of 100,000 14.2 / 33.1 ms.

Blobs (`put_blob` / `blob`, median, two runs): 1 MiB 34 / 5-6 ms, 4 MiB 127-135 / 19 ms, 16 MiB 330-473 / 63-82 ms, 64 MiB 1,683-1,875 / 298-310 ms; writes level off near 30-50 MB/s from 1 MiB on and every value is read and written whole, in memory on both sides. Keep blobs up to 1 MiB in the table; above that, object storage (content-addressed by the same SHA-256) is the better home.

Server settings (`tools/bench.sh`): `shared_buffers=4GB`, `effective_cache_size=11GB`, `work_mem=32MB`, `maintenance_work_mem=1GB`, `wal_buffers=16MB`, `max_wal_size=8GB`, `min_wal_size=1GB`, `checkpoint_completion_target=0.9`, `wal_compression=lz4`, `random_page_cost=1.1`, `effective_io_concurrency=200`, `max_worker_processes=6`, `max_parallel_workers=6`, `max_parallel_workers_per_gather=3`, `max_parallel_maintenance_workers=3`, `max_connections=50`, `jit=off`, `--shm-size=1g`; `fsync` and `synchronous_commit` on.

Licensed under Apache-2.0.
