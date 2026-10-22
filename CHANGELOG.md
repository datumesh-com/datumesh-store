# Changelog

All notable changes to `datumesh-store` and `datumesh-store-cli`.

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
