# API changes in datumesh-store 0.3.1

Generated with `cargo public-api -p datumesh-store -ss diff 0.3.0` (cargo-public-api 0.52.0,
nightly 2026-10-22) against the 0.3.0 release on crates.io; the full output is at the end.
**Additions only: nothing removed or changed.** The crate has no features, so default and
all features give the same API (`tools/public_api.sh`, which also finds no driver or TLS
type outside `pg_unstable`). `public-api.txt` is regenerated.

The new `Store` methods are required ones. That is additive here because `Store` is sealed
(since 0.3.0): no code outside this crate implements it, so no implementation breaks, and
for callers they are new methods. (Each appears twice for `PgStore` in the listing: its
`Store` implementation and the blanket listing of trait methods, as for the 0.3.0 items.)

## Added

| Item | Reason |
|---|---|
| `Store::slice_members(&self, &Slice, asof) -> Result<Vec<SliceMember>>` | The members of a slice as of A (SPEC 15.13), with the stored version answering and its receipt, in key order: what the engine reads for `f(a, *)` and `f(a, [lo..hi])`. |
| `Store::versions_between(&self, cell_id, from, to, asof) -> Result<Vec<SliceMember>>` | A version slice (`versions(c, from, to)` of the dialect): the stored versions in `[from, to]` answering as of A, with receipts. `versions` keeps its meaning (tx only, no receipts). |
| `#[non_exhaustive] pub struct SliceMember { pub version: Version, pub receipt: Vec<u8> }` | One member: the version and its receipt's bytes in one query (the engine reads both). |
| `Store::put_manifest(&self, &[u8]) -> Result<String>` | Stores a manifest under its entry (returned). |
| `Store::member_manifests(&self, receipt_hash) -> Result<Vec<String>>` | The manifests a member belongs to: the readers of a slice found from a member (dirty set, `E_INPUT_SUPERSEDED`). |
| `Store::slice_entries(&self, receipt_hash)`, `Store::slice_dependents(&self, entry)` | The slice-entry lineage, both ways. |
| `PgStore::manifest(&self, &str) -> Option<Cow<'_, [u8]>>` | `datumesh-core` 0.3.4's provided `Resolver::manifest`, overridden with the stored manifest (it was `None`). |
| `PgStore::*` for each `Store` method above | `PgStore`'s implementations. |

## Behaviour (no signature changed; not in the listing)

| Item | Change |
|---|---|
| `put_receipt` | Accepts receipts with slice entries in `inputs` (with core 0.3.4) and records each entry; indexes the receipt's cell by its typed arguments. The manifest is not required. |
| `inputs`, `dependents` | Unchanged: receipt-hash edges only (a slice entry is not among `inputs`). |
| `cell_dependents` | Also the cells reading a cell through a slice whose stored manifest lists one of its receipts. Unchanged for every receipt 0.3.0 could store (none has a slice entry). |
| `put_descriptor`, `Resolver::evaluate` | Descriptors are read as spec-0.4.0's `FunctionDescriptor`: `signature.order` and argument `extent` accepted (0.3.0 refused them with `E_SCHEMA`). A widening: every descriptor 0.3.0 accepted is accepted with the same hash and bytes, and every other descriptor is refused with the core's code. |

## Full diff

```text
$ cargo public-api -p datumesh-store -ss diff 0.3.0
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
(none)

Added items to the public API
=============================
+pub fn datumesh_store::PgStore::manifest(&self, &str) -> core::option::Option<alloc::borrow::Cow<'_, [u8]>>
+pub fn datumesh_store::PgStore::member_manifests(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::member_manifests(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::put_manifest(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::PgStore::put_manifest(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::PgStore::slice_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::slice_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::slice_entries(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::slice_entries(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::PgStore::slice_members(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::PgStore::slice_members(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::PgStore::versions_between(&self, &str, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::PgStore::versions_between(&self, &str, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+#[non_exhaustive] pub struct datumesh_store::SliceMember
+pub datumesh_store::SliceMember::receipt: alloc::vec::Vec<u8>
+pub datumesh_store::SliceMember::version: datumesh_store::Version
+impl core::clone::Clone for datumesh_store::SliceMember
+pub fn datumesh_store::SliceMember::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_store::SliceMember
+impl core::cmp::PartialEq for datumesh_store::SliceMember
+pub fn datumesh_store::SliceMember::eq(&self, &Self) -> bool
+impl core::fmt::Debug for datumesh_store::SliceMember
+pub fn datumesh_store::SliceMember::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::hash::Hash for datumesh_store::SliceMember
+pub fn datumesh_store::SliceMember::hash<__H: core::hash::Hasher>(&self, &mut __H)
+impl core::marker::StructuralPartialEq for datumesh_store::SliceMember
+pub fn datumesh_store::Store::member_manifests(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::Store::put_manifest(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>
+pub fn datumesh_store::Store::slice_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::Store::slice_entries(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>
+pub fn datumesh_store::Store::slice_members(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>
+pub fn datumesh_store::Store::versions_between(&self, &str, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>

```
