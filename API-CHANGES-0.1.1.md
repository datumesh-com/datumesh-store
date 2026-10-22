# API changes in datumesh-store 0.1.1

Generated with `cargo public-api -p datumesh-store -ss diff 0.1.0` (cargo-public-api 0.52.0,
nightly 2026-10-21) against the 0.1.0 release on crates.io; the full output is at the end.
The crate has no features, so default and all features give the same API (checked by
`tools/public_api.sh`). Goal: no type of `postgres`, `tokio-postgres`, `postgres-types`,
`rustls` or any TLS crate in the API outside `datumesh_store::pg_unstable`, which is outside
semver until 1.0. `public-api.txt` is regenerated; after this release the only lines naming
such types are under `datumesh_store::pg_unstable`.

## Removed

| Item | Reason |
|---|---|
| `pub use datumesh_store::postgres` | Callers should not need the driver's types; it is `pg_unstable::postgres` now, outside semver. |
| `impl From<tokio_postgres::error::Error> for Error` (and its `from`) | Named a driver type; the conversion is crate-internal now. |
| `PgStore::with_connector<F>(F, PoolOptions)` | Its connection function returns a `postgres::Client`; moved unchanged to `pg_unstable::with_connector`. `PgStore::connect_with` covers TLS. |

## Changed

| Item | Reason |
|---|---|
| `migrate::apply(&mut postgres::Client)` -> `migrate::apply(&PgStore)` | Takes the store instead of a driver client; holds one pooled connection for the whole apply (advisory lock and tracking-table transactions on one session). A store connects to an empty database without touching it. |
| `migrate::catalog(&mut postgres::Client)` -> `migrate::catalog(&PgStore)` | Same: the store instead of a driver client. |
| `migrate::check(&mut Client, &mut Client)` -> `migrate::check(&PgStore, &PgStore)` | Same: a store on the live database and one on the empty scratch database (the caller still creates and drops it). |
| `Error::Database(tokio_postgres::error::Error)` -> `Error::Database(DatabaseError)` | Named a driver type; `DatabaseError` is this crate's opaque struct with `message()`, `code()` and the driver's error as a type-erased `source()`. Display unchanged. |

## Added

| Item | Reason |
|---|---|
| `PgStore::connect_with(&str, PoolOptions)` | Connect with pool settings and TLS roots, without naming driver types (replaces `with_connector` for the normal cases). |
| `PoolOptions::tls_root_certificates_pem(self, &[u8]) -> Result<PoolOptions>` | Roots to trust besides the Mozilla roots (a private CA, a self-signed server), from a PEM bundle. |
| `PoolOptions::tls_webpki_roots(self, bool)` | Trust only the added roots (private CAs). |
| `Error::Config(String)` | A root certificate bundle with no certificate, or one that does not parse (no existing variant fit: `Invalid` carries the core's protocol error codes). |
| `DatabaseError` (`message`, `code`, `Display`, `Debug`, `Error` with `source`) | The replacement payload of `Error::Database`. |
| `pg_unstable` (module), `pg_unstable::postgres` (re-export) | Outside semver until 1.0: the driver this crate is built with, for building a `Client` for `with_connector`. |
| `pg_unstable::with_connector<F>(F, PoolOptions)` | `PgStore::with_connector`, moved (same signature). |
| `pg_unstable::Connector` (`new(postgres::Config, &PoolOptions)`, `connect()`, `Clone`, `Debug`) | Plain driver clients that connect exactly as the store does (same TLS connector and roots); the command uses it for `export`, the scratch database of `migrate --check` and the benchmark's loading sessions. |

## Full diff

```text
Removed items from the public API
=================================
-pub use datumesh_store::postgres
-impl core::convert::From<tokio_postgres::error::Error> for datumesh_store::Error
-pub fn datumesh_store::Error::from(tokio_postgres::error::Error) -> datumesh_store::Error
-pub fn datumesh_store::PgStore::with_connector<F>(F, datumesh_store::PoolOptions) -> datumesh_store::Result<datumesh_store::PgStore> where F: core::ops::function::Fn() -> core::result::Result<postgres::client::Client, tokio_postgres::error::Error> + core::marker::Send + core::marker::Sync + 'static

Changed items in the public API
===============================
-pub fn datumesh_store::migrate::apply(&mut postgres::client::Client) -> datumesh_store::Result<alloc::vec::Vec<u32>>
+pub fn datumesh_store::migrate::apply(&datumesh_store::PgStore) -> datumesh_store::Result<alloc::vec::Vec<u32>>
-pub fn datumesh_store::migrate::catalog(&mut postgres::client::Client) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::migrate::catalog(&datumesh_store::PgStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
-pub fn datumesh_store::migrate::check(&mut postgres::client::Client, &mut postgres::client::Client) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::migrate::check(&datumesh_store::PgStore, &datumesh_store::PgStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
-pub datumesh_store::Error::Database(tokio_postgres::error::Error)
+pub datumesh_store::Error::Database(datumesh_store::DatabaseError)

Added items to the public API
=============================
+pub mod datumesh_store::pg_unstable
+pub use datumesh_store::pg_unstable::postgres
+pub struct datumesh_store::pg_unstable::Connector
+impl datumesh_store::pg_unstable::Connector
+pub fn datumesh_store::pg_unstable::Connector::connect(&self) -> core::result::Result<postgres::client::Client, tokio_postgres::error::Error>
+pub fn datumesh_store::pg_unstable::Connector::new(postgres::config::Config, &datumesh_store::PoolOptions) -> datumesh_store::Result<datumesh_store::pg_unstable::Connector>
+impl core::clone::Clone for datumesh_store::pg_unstable::Connector
+pub fn datumesh_store::pg_unstable::Connector::clone(&self) -> Self
+impl core::fmt::Debug for datumesh_store::pg_unstable::Connector
+pub fn datumesh_store::pg_unstable::Connector::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+pub fn datumesh_store::pg_unstable::with_connector<F>(F, datumesh_store::PoolOptions) -> datumesh_store::Result<datumesh_store::PgStore> where F: core::ops::function::Fn() -> core::result::Result<postgres::client::Client, tokio_postgres::error::Error> + core::marker::Send + core::marker::Sync + 'static
+pub datumesh_store::Error::Config(alloc::string::String)
+pub struct datumesh_store::DatabaseError
+impl datumesh_store::DatabaseError
+pub fn datumesh_store::DatabaseError::code(&self) -> core::option::Option<&str>
+pub fn datumesh_store::DatabaseError::message(&self) -> &str
+impl core::error::Error for datumesh_store::DatabaseError
+pub fn datumesh_store::DatabaseError::source(&self) -> core::option::Option<&(dyn core::error::Error + 'static)>
+impl core::fmt::Debug for datumesh_store::DatabaseError
+pub fn datumesh_store::DatabaseError::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+pub fn datumesh_store::DatabaseError::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::fmt::Display for datumesh_store::DatabaseError
+pub fn datumesh_store::PgStore::connect_with(&str, datumesh_store::PoolOptions) -> datumesh_store::Result<datumesh_store::PgStore>
+pub fn datumesh_store::PoolOptions::tls_root_certificates_pem(self, &[u8]) -> datumesh_store::Result<datumesh_store::PoolOptions>
+pub fn datumesh_store::PoolOptions::tls_webpki_roots(self, bool) -> datumesh_store::PoolOptions

```
