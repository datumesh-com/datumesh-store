# API changes in datumesh-store 0.4.0

Generated with `cargo public-api -p datumesh-store -ss diff 0.3.2` (cargo-public-api 0.52.0,
nightly 2026-10-22) against the 0.3.2 release on crates.io; the full output is at the end.
**Additions only: nothing removed or changed. Nothing breaks for users of `PgStore`.**
Default and all features give the same API (the one feature, `sqlite`, is on by default);
no type of either driver (`postgres`, `rusqlite`) or of a TLS crate outside `pg_unstable`
(`tools/public_api.sh`, `tests/public_api.rs`). `public-api.txt` is regenerated.

This is 0.4.0, not 0.3.3, because the sealed `Store` gains a second implementation (the
brief: "store 0.4.0, since the trait gains a backend"); `Store` itself is unchanged.

## Added

| Item | Reason |
|---|---|
| `pub struct SqliteStore` with `open(path)`, `open_with(path, SqliteOptions)`, `connect(url)` (`sqlite:<path>`, `sqlite://<path>`, or a path), `path()`, `check_statements()` | The second backend: one SQLite file, no server, for the local resolver (`datumesh-local`). The same constructors' shape as `PgStore` (`connect` / `connect_with`, `check_statements`). `open` creates a missing file (not its directory). |
| `SqliteStore::open_existing(path)` | Opens only an existing file and changes nothing about it (no file created, journal mode left as it is); a missing path is `Error::Database` "no such database file: <path>" (`SQLITE_CANTOPEN`). For checks, which have no side effects. |
| `impl Store for SqliteStore`, `impl Resolver for SqliteStore` (every method, as listed below) | The same behaviour as `PgStore`, checked by one conformance suite run against both. |
| `pub struct SqliteOptions` (`new`, `Default`, `max_size`, `busy_timeout`, `durable`; `Clone`, `Debug`, `Eq`) | Idle connections kept; how long a writer waits for the database's write lock (default 30 s, as `PoolOptions::checkout_timeout`); durable commits (`synchronous = FULL`, default) or not (`NORMAL`, for bulk loads, like `synchronous_commit = off`). `PoolOptions` is PostgreSQL's (TLS roots), so SQLite has its own. |
| `pub mod migrate::sqlite` with `MIGRATIONS`, `apply(&SqliteStore)`, `catalog(&SqliteStore)`, `check(&SqliteStore)`, `check_file(path)` (`check` through `open_existing`: creates nothing) | The `migrate` / `migrate --check` contract for SQLite. `check` takes no scratch store: it migrates a scratch database in memory. The PostgreSQL functions keep their signatures (`&PgStore`), so no caller changes. |
| feature `sqlite` (default) | `cargo add datumesh-store` keeps working and gets both backends; `default-features = false` builds the PostgreSQL store alone (no C compilation of SQLite). |

Not visible in the listing, but worth naming:

- `DatabaseError::code()` returns, for a SQLite error, the name of SQLite's extended result
  code (`SQLITE_BUSY`, `SQLITE_CONSTRAINT_TRIGGER`, `SQLITE_CANTOPEN`, ...); for PostgreSQL
  the SQLSTATE as before. `DatabaseError`'s private source field became `Box<dyn Error +
  Send + Sync>`; `source()` returns the driver's error as before.
- The crate's build now compiles SQLite (C) by default; `ring` already required a C
  compiler, so the toolchain requirement is unchanged. MSRV unchanged (1.97).

## Full diff

```text
$ cargo public-api -p datumesh-store -ss diff 0.3.2
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
(none)

Added items to the public API
=============================
+pub mod datumesh_store::migrate::sqlite
+pub const datumesh_store::migrate::sqlite::MIGRATIONS: &[datumesh_store::migrate::Migration]
+pub fn datumesh_store::migrate::sqlite::apply(&datumesh_store::SqliteStore) -> datumesh_store::Result<alloc::vec::Vec<u32>>
+pub fn datumesh_store::migrate::sqlite::catalog(&datumesh_store::SqliteStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::migrate::sqlite::check(&datumesh_store::SqliteStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::migrate::sqlite::check_file(impl core::convert::AsRef<std::path::Path>) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub struct datumesh_store::SqliteOptions
+impl datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::busy_timeout(self, core::time::Duration) -> datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::durable(self, bool) -> datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::max_size(self, usize) -> datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::new() -> datumesh_store::SqliteOptions
+impl core::clone::Clone for datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_store::SqliteOptions
+impl core::cmp::PartialEq for datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::eq(&self, &Self) -> bool
+impl core::default::Default for datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::default() -> datumesh_store::SqliteOptions
+impl core::fmt::Debug for datumesh_store::SqliteOptions
+pub fn datumesh_store::SqliteOptions::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::marker::StructuralPartialEq for datumesh_store::SqliteOptions
+pub struct datumesh_store::SqliteStore
+impl datumesh_store::SqliteStore
+pub fn datumesh_store::SqliteStore::check_statements(&self) -> datumesh_store::Result<usize>
+pub fn datumesh_store::SqliteStore::connect(&str) -> datumesh_store::Result<datumesh_store::SqliteStore>
+pub fn datumesh_store::SqliteStore::open(impl core::convert::AsRef<std::path::Path>) -> datumesh_store::Result<datumesh_store::SqliteStore>
+pub fn datumesh_store::SqliteStore::open_existing(impl core::convert::AsRef<std::path::Path>) -> datumesh_store::Result<datumesh_store::SqliteStore>
+pub fn datumesh_store::SqliteStore::open_with(impl core::convert::AsRef<std::path::Path>, datumesh_store::SqliteOptions) -> datumesh_store::Result<datumesh_store::SqliteStore>
+pub fn datumesh_store::SqliteStore::path(&self) -> &std::path::Path
+impl core::fmt::Debug for datumesh_store::SqliteStore
+pub fn datumesh_store::SqliteStore::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl datumesh_core::verify::Resolver for datumesh_store::SqliteStore
+pub fn datumesh_store::SqliteStore::descriptor(&self, &str) -> core::option::Option<alloc::borrow::Cow<'_, [u8]>>
+pub fn datumesh_store::SqliteStore::evaluate(&self, &datumesh_core::model::Cell, &str) -> core::option::Option<datumesh_core::model::Scalar>
+pub fn datumesh_store::SqliteStore::manifest(&self, &str) -> core::option::Option<alloc::borrow::Cow<'_, [u8]>>
+pub fn datumesh_store::SqliteStore::receipt(&self, &str) -> core::option::Option<alloc::borrow::Cow<'_, [u8]>>
+impl datumesh_store::Store for datumesh_store::SqliteStore
+impl datumesh_store::Store for datumesh_store::SqliteStore
+pub fn datumesh_store::SqliteStore::bind_name(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::bind_name(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::blob(&self, &str) -> datumesh_store::Result<core::option::Option<alloc::borrow::Cow<'_, [u8]>>>
+pub fn datumesh_store::SqliteStore::blob(&self, &str) -> datumesh_store::Result<core::option::Option<alloc::borrow::Cow<'_, [u8]>>>
+pub fn datumesh_store::SqliteStore::cell_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::cell_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::computed_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::computed_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::computed_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::SqliteStore::computed_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::SqliteStore::dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::inputs(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::inputs(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::latest(&self, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::latest(&self, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::member_manifests(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::member_manifests(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::namespace(&self, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Namespace>>
+pub fn datumesh_store::SqliteStore::namespace(&self, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Namespace>>
+pub fn datumesh_store::SqliteStore::put_blob(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_blob(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_computed(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::put_computed(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::put_descriptor(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_descriptor(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_manifest(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_manifest(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_namespace(&self, &str, &[u8], &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::put_namespace(&self, &str, &[u8], &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::put_receipt(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_receipt(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::SqliteStore::put_version(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::put_version(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::SqliteStore::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>
+pub fn datumesh_store::SqliteStore::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>
+pub fn datumesh_store::SqliteStore::resolve_name(&self, &str, &str) -> datumesh_store::Result<core::option::Option<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::resolve_name(&self, &str, &str) -> datumesh_store::Result<core::option::Option<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::slice_changed_after(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<bool>
+pub fn datumesh_store::SqliteStore::slice_changed_after(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<bool>
+pub fn datumesh_store::SqliteStore::slice_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::slice_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::slice_entries(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::slice_entries(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::SqliteStore::slice_members(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::SqliteStore::slice_members(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::SqliteStore::slice_values(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>
+pub fn datumesh_store::SqliteStore::slice_values(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>
+pub fn datumesh_store::SqliteStore::stored_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::stored_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::stored_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::SqliteStore::stored_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::SqliteStore::version_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::version_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::versions(&self, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::versions(&self, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::Version>>
+pub fn datumesh_store::SqliteStore::versions_between(&self, &str, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::SqliteStore::versions_between(&self, &str, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>

```
