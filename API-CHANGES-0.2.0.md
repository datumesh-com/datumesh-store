# API changes in datumesh-store 0.2.0

Generated with `cargo public-api -p datumesh-store -ss diff 0.1.2` (cargo-public-api 0.52.0,
nightly 2026-10-21) against the 0.1.2 release on crates.io; the full output is at the end.
Additions only: nothing removed or changed. The crate has no features, so default and all
features give the same API (checked by `tools/public_api.sh`, which also finds no driver or
TLS type outside `pg_unstable`). `public-api.txt` is regenerated.

Adding methods to the public `Store` trait is breaking for other implementors of `Store`
(the four new required methods), which the minor version 0.2.0 covers; for callers every
change is an addition. `Version` is `#[non_exhaustive]`, so its new field is additive.

## Added

| Item | Reason |
|---|---|
| `Store::put_computed(&self, cell_id, tx, receipt_hash) -> Result<()>` | Records a computed version of a cell: insert-only, any order, keyed on (cell_id, tx, asof), the asof being the receipt's. Idempotent; another receipt at an existing key is `Error::Conflict`; the receipt is checked as by `put_version`. Required. |
| `Store::computed_asof(&self, cell_id, asof) -> Result<Option<Version>>` | The computed version answering as of A: the greatest asof <= A, ties the greatest tx. Required. |
| `Store::stored_asof(&self, cell_id, asof) -> Result<Option<Version>>` | The stored version answering as of A: latest by tx with tx <= A and receipt asof <= A (`version_asof` keeps its tx-only meaning). Required. |
| `Store::cell_dependents(&self, cell_id) -> Result<Vec<String>>` | The cells whose stored receipts read any stored receipt of the cell (edges joined to receipts), ascending, unique. Required. |
| `Store::computed_asof_with_receipt(&self, cell_id, asof) -> Result<Option<(Version, Cow<'_, [u8]>)>>` | `computed_asof` and the answer's receipt bytes, which the engine reads next. Provided (default: the two calls in turn), so not breaking by itself; `PgStore` answers in one query, about 45 % faster than the two calls (bench/results/2026-10-24). |
| `Store::stored_asof_with_receipt(&self, cell_id, asof) -> Result<Option<(Version, Cow<'_, [u8]>)>>` | The same for `stored_asof`. Provided. |
| `PgStore::{put_computed, computed_asof, stored_asof, cell_dependents, computed_asof_with_receipt, stored_asof_with_receipt}` | `PgStore`'s implementations of the above. |
| `Version::asof: String` | The version's receipt's asof, filled by every method returning a `Version` (`version_asof`, `versions`, `latest` too). |

## Full diff

```text
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
(none)

Added items to the public API
=============================
+pub fn datumesh_store::PgStore::cell_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::cell_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::computed_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::PgStore::computed_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::PgStore::computed_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::PgStore::computed_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::PgStore::put_computed(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::PgStore::put_computed(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::PgStore::stored_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::PgStore::stored_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::PgStore::stored_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::PgStore::stored_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub datumesh_store::Version::asof: alloc::string::String
+pub fn datumesh_store::Store::cell_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::Store::computed_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::Store::computed_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>
+pub fn datumesh_store::Store::put_computed(&self, &str, &str, &str) -> datumesh_store::Result<()>
+pub fn datumesh_store::Store::stored_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>
+pub fn datumesh_store::Store::stored_asof_with_receipt(&self, &str, &str) -> datumesh_store::Result<core::option::Option<(datumesh_store::Version, alloc::borrow::Cow<'_, [u8]>)>>

```
