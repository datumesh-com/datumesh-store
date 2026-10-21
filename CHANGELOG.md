# Changelog

All notable changes to `datumesh-store` and `datumesh-store-cli`.

## 0.1.0 (unreleased)

First release, for `datumesh-core` 0.3.1 (specification `spec-0.2.1`).

- `Store` trait (a `datumesh_core::Resolver` that can be written to) and `PgStore` on
  PostgreSQL 16: descriptors and receipts checked by the core and stored canonical; blobs;
  append-only versions over transaction time and name bindings, enforced by triggers and
  serialised per cell / per name; lineage edges; namespaces' key documents. Passes the
  core's resolver test kit as a time-aware resolver.
- Synchronous API (`postgres` driver) with the crate's own small connection pool and
  per-connection prepared statements; typed `Error`; reads answer `None` / empty and log
  database errors with `tracing`.
- `migrate`: embedded SQL migrations with a tracking table, a catalog dump and a schema
  check against a scratch database.
- `datumesh-store` command: `migrate [--check]`, `import`, `export`, `bench`.
