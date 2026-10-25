# API changes in datumesh-store 0.3.2

Generated with `cargo public-api -p datumesh-store -ss diff 0.3.1` (cargo-public-api 0.52.0,
nightly 2026-10-22) against the 0.3.1 release on crates.io; the full output is at the end.
**Additions only: nothing removed or changed.** Default and all features give the same API;
no driver or TLS type outside `pg_unstable` (`tools/public_api.sh`). `public-api.txt` is
regenerated. The two `Store` methods are required ones, additive because `Store` is sealed.

## Added

| Item | Reason |
|---|---|
| `Store::slice_values(&self, &Slice, asof) -> Result<Vec<SliceValue>>` | The members `slice_members` gives (same order, same answering rule) with each member's typed value and cell, and no receipt bytes: the engine evaluates over values and hands the members' key arguments to the runtime, and needs receipts only for what it signs or returns. Values come from a stored column of `receipts` (migration 4), cells from the typed-argument index. |
| `#[non_exhaustive] pub struct SliceValue { pub version: Version, pub value: Scalar, pub cell: Cell }` | One member: its version (cell id, tx, receipt hash, asof), its value, its cell with its arguments (`cell.id() == version.cell_id`). |
| `Store::slice_changed_after(&self, &Slice, asof) -> Result<bool>` | Whether a matching cell has a stored version whose receipt's asof is after A (its tx is never later than its asof): the engine's check that a slice answers the same as of every later time, with index scans only (two bounded probes choose between the function's recent receipts and the slice's candidate cells; one query walks the smaller) instead of a second scan of the members at the end of time. Conservative: a late-signed version superseded before its asof gives `true`. |
| `PgStore::slice_values`, `PgStore::slice_changed_after` | `PgStore`'s implementations. |

Both take the slice object (`datumesh_core::Slice`), not its id, as `slice_members` does:
the id is a hash, and the store holds a slice object only where a manifest of it is stored,
which the engine's first read of a slice does not have.

## Full diff

```text
$ cargo public-api -p datumesh-store -ss diff 0.3.1
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
(none)

Added items to the public API
=============================
+pub fn datumesh_store::PgStore::slice_changed_after(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<bool>
+pub fn datumesh_store::PgStore::slice_changed_after(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<bool>
+pub fn datumesh_store::PgStore::slice_values(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>
+pub fn datumesh_store::PgStore::slice_values(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>
+#[non_exhaustive] pub struct datumesh_store::SliceValue
+pub datumesh_store::SliceValue::cell: datumesh_core::model::Cell
+pub datumesh_store::SliceValue::value: datumesh_core::model::Scalar
+pub datumesh_store::SliceValue::version: datumesh_store::Version
+impl core::clone::Clone for datumesh_store::SliceValue
+pub fn datumesh_store::SliceValue::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_store::SliceValue
+impl core::cmp::PartialEq for datumesh_store::SliceValue
+pub fn datumesh_store::SliceValue::eq(&self, &Self) -> bool
+impl core::fmt::Debug for datumesh_store::SliceValue
+pub fn datumesh_store::SliceValue::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::hash::Hash for datumesh_store::SliceValue
+pub fn datumesh_store::SliceValue::hash<__H: core::hash::Hasher>(&self, &mut __H)
+impl core::marker::StructuralPartialEq for datumesh_store::SliceValue
+pub fn datumesh_store::Store::slice_changed_after(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<bool>
+pub fn datumesh_store::Store::slice_values(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>

```
