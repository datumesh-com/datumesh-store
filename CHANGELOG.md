# Changelog

All notable changes to `datumesh-store` and `datumesh-store-cli`.

## 0.1.2 (2026-10-24)

- Requires `datumesh-core` 0.3.2 (was 0.3.1), which implements specification `spec-0.3.0`:
  an input receipt's `asof` may be at or before the receipt's, an `ok` envelope's receipt
  `asof` at or before the requested one, and `E_EVAL_UNAVAILABLE` when a function cannot be
  evaluated. The tests run the `spec-0.3.0` vectors. No API change.
- The first release published with crates.io Trusted Publishing.

## 0.1.1 (2026-10-22)

No type of the `postgres` driver or of a TLS crate is part of the API any more, except in
the new module `pg_unstable`, which is outside semver until 1.0. TLS through `PoolOptions`.
Published with an API token: Trusted Publishing was refused on the `v0.1.1` run (see 0.1.2).

### Changed (breaking for code that named `postgres` types; 0.1.0 had 16 downloads)

- `pub use postgres` at the crate root: removed. The driver is re-exported as
  `pg_unstable::postgres`, outside semver.
- `migrate::apply(&mut postgres::Client)`, `migrate::catalog(&mut postgres::Client)`,
  `migrate::check(&mut Client, &mut Client)`: they take the store instead,
  `migrate::apply(&PgStore)`, `migrate::catalog(&PgStore)`,
  `migrate::check(live: &PgStore, scratch: &PgStore)`. `apply` holds one pooled connection
  throughout (the advisory lock and the tracking table on one session). A `PgStore` connects
  to an empty database without touching it, so `migrate::apply(&PgStore::connect(url)?)`
  makes the schema.
- `Error::Database(postgres::Error)`: now `Error::Database(DatabaseError)`, an opaque struct
  with `message()` (the server's message, or the driver's text with its causes), `code()`
  (the SQLSTATE, e.g. `42P01`) and the driver's error as `source()` (type erased). Display
  is as before (`database: <message> (<SQLSTATE>)`). For a failure that is not the
  server's, the message now includes the driver's causes (`error performing TLS handshake:
  invalid peer certificate: UnknownIssuer` rather than `error performing TLS handshake`).
- `impl From<postgres::Error> for Error`: removed (the conversion is internal).
- `PgStore::with_connector(f, PoolOptions)`: moved to `pg_unstable::with_connector(f,
  PoolOptions)`, same signature, outside semver. `PgStore::connect_with` covers TLS.
- `PgStore::connect(url)` now uses TLS when the server offers it (the default
  `sslmode=prefer`), verifying the certificate: a server with `ssl=on` and a certificate
  the Mozilla roots do not cover now fails to connect; add its CA, or use
  `sslmode=disable`. In 0.1.0 it never used TLS.

### Added

- TLS: `PgStore::connect_with(url, PoolOptions)`; `connect` and `connect_with` use rustls
  (`tokio-postgres-rustls` 0.14, `rustls` 0.23 with the ring provider chosen per connector,
  no process-wide provider) as the URL's `sslmode` says (`disable`, `prefer`, `require`),
  verifying the server's certificate and host name against the Mozilla roots
  (`webpki-roots`) plus `PoolOptions::tls_root_certificates_pem(pem)`;
  `PoolOptions::tls_webpki_roots(false)` trusts only the added roots. No option accepts any
  certificate.
- `Error::Config(String)`: a root certificate bundle with no certificate, or one that does
  not parse.
- `DatabaseError` (see above).
- `pg_unstable` (outside semver until 1.0): `with_connector`, `Connector` (plain
  `postgres` clients that connect as the store does), `pub use postgres`.
- The command: TLS for every subcommand (`DATABASE_URL`'s `sslmode`), `DATABASE_CA_FILE`
  for roots to trust besides the Mozilla roots; connection errors show their causes. It
  depends on `datumesh-store = "=0.1.1"` exactly (it uses `pg_unstable`).

## 0.1.0 (2026-10-22)

First release, for `datumesh-core` 0.3.1 (specification `spec-0.2.1`).

- `Store` trait (a `datumesh_core::Resolver` that can be written to) and `PgStore` on
  PostgreSQL 16: descriptors checked by the core and stored canonical; receipts verified
  (SPEC 10.1 steps 1-6) against the key document stored for their signer host and stored
  canonical, so nothing unverified enters the store (putting a stored receipt again is
  idempotent and not re-checked, even after its key is revoked); blobs;
  append-only versions over transaction time and name bindings, enforced by triggers and
  serialised per cell / per name; lineage edges; namespaces' key documents. Passes the
  core's resolver test kit as a time-aware resolver.
- Synchronous API (`postgres` driver) with the crate's own small connection pool and
  per-connection prepared statements; typed `Error`. Every `Store` method, reads included,
  returns a `Result` (malformed arguments are `Error::Invalid`, database failures
  `Error::Database`); the `Resolver` methods keep `Option` and log failures with `tracing`.
- `migrate`: embedded SQL migrations with a tracking table, a catalog dump and a schema
  check against a scratch database.
- `datumesh-store` command: `migrate [--check]`, `import`, `export`, `bench`.
