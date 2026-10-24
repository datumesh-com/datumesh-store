# API changes in datumesh-store 0.3.0

Generated with `cargo public-api -p datumesh-store -ss diff 0.2.0` (cargo-public-api 0.52.0,
nightly 2026-10-21) against the 0.2.0 release on crates.io; the full output is at the end.
The crate has no features, so default and all features give the same API (checked by
`tools/public_api.sh`, which also finds no driver or TLS type outside `pg_unstable`).
`public-api.txt` is regenerated.

Why 0.3.0 and not 0.2.1: code outside this crate *can* implement `Store` up to 0.2.0. The
`#[non_exhaustive]` on `Version` and `Namespace` only forbids struct literals: an
implementation can answer `Ok(None)` / `Ok(vec![])`, build any `Error` variant, delegate to
a `PgStore`, or clone a `Version` a `PgStore` returned and set its public fields
(`datumesh-engine`'s test stores `MemStore` and `Counting` do exactly that). So a new
required method breaks them, and so does the seal.

## Changed (breaking)

| Item | Reason |
|---|---|
| `pub trait Store: Resolver` → `pub trait Store: Resolver + sealed::Sealed` | **Sealed**: a public trait in a private module, implemented for `PgStore` only, so no other crate can implement `Store`. The store is the trust boundary (every receipt verified against its host's key document before it is kept; callers rely on its answers as checked). `Store` is still object-safe: `Arc<dyn Store + Send + Sync>` works as before (`tests/sealed.rs` builds an outside crate that uses it, and one that implements it, which fails with `E0277` for the seal only). Breaking for code outside the crate that implemented `Store`. |
| `Store::resolve_binding(&self, fn_name, asof) -> Result<Option<Binding>>` (new, **required**) | The binding of a name that answers as of A: the latest `tx <= A`, with its function hash and its `tx` (when it began); a name bound again begins again. The engine needs the start of every binding a computed receipt relied on for its asof. A required method is breaking for other implementors; after the seal there are none. |
| `Resolver::evaluate` for `PgStore` (behaviour; the signature is the core's) | The value of a stored function as of A is that of `stored_asof`'s version (`tx <= A` and the receipt's `asof <= A`), not `version_asof`'s (`tx <= A`). Differs only for late-signed receipts (asof later than tx): none of their value before their asof. The core 0.3.3 testkit's default `AnswerRule::TxAndAsof` (revised D31). `version_asof` is unchanged. Not in the listing below (no item changed). |

## Added

| Item | Reason |
|---|---|
| `#[non_exhaustive] pub struct Binding { pub fn_hash: String, pub tx: String }` (Clone, Debug, PartialEq, Eq, Hash) | The answer of `resolve_binding`. `#[non_exhaustive]`, like `Version`, so fields can be added. |
| `PgStore::resolve_binding` | `PgStore`'s implementation (one lookup on the `name_bindings` primary key). |

Nothing removed. `resolve_name` is unchanged (it is `resolve_binding`'s `fn_hash`).

## Full diff

```text
Removed items from the public API
=================================
(none)

Changed items in the public API
===============================
-pub trait datumesh_store::Store: datumesh_core::verify::Resolver
+pub trait datumesh_store::Store: datumesh_core::verify::Resolver + datumesh_store::sealed::Sealed

Added items to the public API
=============================
+#[non_exhaustive] pub struct datumesh_store::Binding
+pub datumesh_store::Binding::fn_hash: alloc::string::String
+pub datumesh_store::Binding::tx: alloc::string::String
+impl core::clone::Clone for datumesh_store::Binding
+pub fn datumesh_store::Binding::clone(&self) -> Self
+impl core::cmp::Eq for datumesh_store::Binding
+impl core::cmp::PartialEq for datumesh_store::Binding
+pub fn datumesh_store::Binding::eq(&self, &Self) -> bool
+impl core::fmt::Debug for datumesh_store::Binding
+pub fn datumesh_store::Binding::fmt(&self, &mut core::fmt::Formatter<'_>) -> core::fmt::Result
+impl core::hash::Hash for datumesh_store::Binding
+pub fn datumesh_store::Binding::hash<__H: core::hash::Hasher>(&self, &mut __H)
+impl core::marker::StructuralPartialEq for datumesh_store::Binding
+pub fn datumesh_store::PgStore::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>
+pub fn datumesh_store::PgStore::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>
+pub fn datumesh_store::Store::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>

```
