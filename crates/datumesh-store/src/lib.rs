//! A PostgreSQL store for [Datumesh](https://datumesh.com): function descriptors, receipts,
//! blobs, versions of cells over transaction time, lineage, namespaces and function names.
//! [`PgStore`] implements [`Store`], and through it `datumesh_core`'s [`Resolver`], so
//! `datumesh_core::verify` can recompute receipts of stored cells from it.
//!
//! The API is synchronous (the `postgres` driver, a small connection pool). Async callers run
//! it on a blocking thread, e.g. `tokio::task::spawn_blocking`.
//!
//! [`PgStore::connect`] / [`PgStore::connect_with`] take a URL and [`PoolOptions`]; TLS
//! (rustls) is used as the URL's `sslmode` says and always verifies the server's
//! certificate and host name, against the Mozilla roots plus those added with
//! [`PoolOptions::tls_root_certificates_pem`]. No type of the driver or of a TLS crate is
//! part of the API, except in [`pg_unstable`] (any connection function, the driver
//! itself), which is outside semver until 1.0.
//!
//! What is stored, and how:
//!
//! * Descriptors are checked by the core (`FunctionDescriptor::from_json`: since 0.3.1,
//!   spec-0.4.0's, with `signature.order` and argument `extent`) and stored in canonical
//!   form (JCS) under their hash. Receipts are verified (`datumesh_core::verify`, SPEC 10.1
//!   steps 1-6) against the key document stored for their signer host, so nothing
//!   unverified enters the store, and are stored canonical under their hash. Storing the
//!   same thing twice stores it once. A second, different valid signature over the same
//!   payload (possible only with a randomised signer: the payload names the key, so a
//!   rotated key gives a new hash) is refused ([`Error::Conflict`]) and the stored receipt
//!   kept.
//! * Blobs are stored in the database, under the SHA-256 of their content.
//! * Versions: from transaction time `tx` on, cell `cell_id` has the value of a receipt.
//!   Append-only and in tx order per cell, enforced by the database (triggers) as well as by
//!   [`Store::put_version`], which also checks that the receipt is stored and is of that
//!   cell at that tx.
//! * Computed versions: from transaction time `tx` on, cell `cell_id` has the value a
//!   receipt computed, as of the receipt's asof. Derived rather than facts, so they are
//!   added in any order, keyed on (cell, tx, asof), insert-only
//!   ([`Store::put_computed`], [`Store::computed_asof`]).
//! * Answering as of A: a receipt answers as of A only when its `tx` and its `asof` are
//!   both at or before A ([`Store::stored_asof`], and the [`Resolver`]'s `evaluate` of a
//!   stored function; [`Store::version_asof`] looks at `tx` only).
//! * Lineage: `put_receipt` records an edge to each of the receipt's `inputs`, held or not:
//!   receipt hashes ([`Store::inputs`], [`Store::dependents`]) and, since 0.3.1, slice
//!   entries ([`Store::slice_entries`], [`Store::slice_dependents`]);
//!   [`Store::cell_dependents`] lifts both to cells.
//! * Slices (0.3.1, SPEC 15.13): every stored receipt's cell is indexed by its typed
//!   arguments, so that [`Store::slice_members`] finds the members of a slice as of A with
//!   a range scan, in key order; [`Store::versions_between`] gives a version slice.
//!   [`Store::slice_values`] gives the members' values without receipts (0.3.2) and
//!   [`Store::slice_changed_after`] whether they may answer differently later.
//!   Manifests ([`Store::put_manifest`], served by [`Resolver::manifest`]) are kept by
//!   entry and found from each member ([`Store::member_manifests`]). A receipt citing a
//!   slice can be stored before its manifest, and a manifest before its members.
//! * Namespaces: a host's key document (checked as SPEC 9.2 requires), with when it was
//!   validated. Store it before the host's receipts. Replacing it later (a key revoked or
//!   dropped) does not touch receipts already stored: they verified when they were stored,
//!   and putting one of them again is still idempotent.
//! * Names: which function hash a fully qualified function name meant from a transaction
//!   time on. Append-only, like versions. [`Store::resolve_binding`] gives the binding
//!   answering as of A with when it began; [`Store::resolve_name`] its hash.
//!
//! Instants are compared as text (fixed width, `COLLATE "C"`), which is exact; each is also
//! stored as `timestamptz`, which round-trips every instant of the specification exactly.
//!
//! Every [`Store`] method returns a `Result`: a malformed argument is [`Error::Invalid`] with
//! the core's code, a database failure [`Error::Database`]. The [`Resolver`] methods keep the
//! core's `Option` signatures; there a failure is logged with `tracing` (target
//! `datumesh_store`) and answered `None`, which `verify` reports as unavailable.
//!
//! [`Store`] is sealed: only this crate implements it. The store is the trust boundary:
//! everything in it was checked on the way in (every receipt verified against its host's
//! key document), and its callers rely on that rather than checking again. A `dyn Store`
//! is still a trait object as before.
//!
//! The schema comes from [`migrate::apply`] (the `datumesh-store migrate` command), through
//! a store on the (possibly empty) database.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::borrow::Cow;

pub use datumesh_core::Resolver;
use datumesh_core::{Cell, Scalar, Slice};

mod error;
pub mod migrate;
mod pg;
pub mod pg_unstable;
mod pool;
mod sql;
mod tls;

pub use error::{DatabaseError, Error, Result};
pub use pg::PgStore;
pub use pool::PoolOptions;

/// A version of a cell: from transaction time `tx` on, the cell's value is that of the
/// receipt `receipt_hash`, which states it as of `asof`.
///
/// A stored version ([`Store::put_version`]) or a computed one ([`Store::put_computed`]).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    /// The cell id.
    pub cell_id: String,
    /// The transaction time (an instant, SPEC 3.3).
    pub tx: String,
    /// The receipt stating the value.
    pub receipt_hash: String,
    /// The receipt's `asof` (an instant; never earlier than `tx`). Since 0.2.0.
    pub asof: String,
}

/// A member of a slice ([`Store::slice_members`], [`Store::versions_between`]): a stored
/// version and the bytes of its receipt (as [`receipt`](Resolver::receipt) returns them).
/// Since 0.3.1.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SliceMember {
    /// The version answering (its cell, tx, receipt hash and asof).
    pub version: Version,
    /// The receipt's bytes, canonical JSON.
    pub receipt: Vec<u8>,
}

/// A member of a slice with its value and without its receipt's bytes
/// ([`Store::slice_values`]). Since 0.3.2.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SliceValue {
    /// The version answering (its cell, tx, receipt hash and asof).
    pub version: Version,
    /// The receipt's typed value.
    pub value: Scalar,
    /// The member's cell, with its arguments (from the store's typed-argument index; equal
    /// to the receipt's `cell`, and `cell.id()` is `version.cell_id`).
    pub cell: Cell,
}

/// A name binding: from transaction time `tx` on, a fully qualified function name means the
/// function `fn_hash` ([`Store::bind_name`]). Since 0.3.0.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    /// The function hash the name means.
    pub fn_hash: String,
    /// When the binding began: the transaction time from which the name means `fn_hash`
    /// (an instant, SPEC 3.3).
    pub tx: String,
}

/// A namespace's host and its key document.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Namespace {
    /// The host (a DNS name).
    pub host: String,
    /// The key document, canonical (JCS).
    pub keydoc: Vec<u8>,
    /// When the document was last retrieved and checked.
    pub validated_at: String,
    /// When the namespace was suspended, if it is. Nothing in this version sets it.
    pub suspended_at: Option<String>,
}

/// A Datumesh store: a [`Resolver`] that can also be written to.
///
/// Instants are SPEC 3.3 instants (`YYYY-MM-DDTHH:MM:SS.ffffffZ`), hashes and cell ids 64
/// lowercase hex digits, hosts DNS names and function names fully qualified. Every method,
/// read or write, refuses anything else with the core's error code ([`Error::Invalid`]),
/// and reports a database failure as [`Error::Database`] (or [`Error::PoolTimeout`]): a read
/// answers `Ok(None)` / `Ok(vec![])` only when the store really holds nothing.
///
/// The [`Resolver`] methods keep the core's `Option` signatures: there a malformed argument
/// or a database failure is `None` (logged with `tracing` when it is a failure), which
/// `verify` reports as unavailable.
///
/// Only this crate implements `Store` (it is sealed, since 0.3.0): the store is the trust
/// boundary, where every receipt is verified against its host's key document before it is
/// kept, and what a store answers is relied on as checked. An implementation elsewhere
/// could answer anything. `Store` stays object-safe (`Arc<dyn Store + Send + Sync>`). Even a complete implementation that
/// delegates every method to a [`PgStore`] is refused (`E0277`: the private supertrait is
/// not implemented; `tests/sealed.rs` checks that this is the only error):
///
/// ```compile_fail
/// use std::borrow::Cow;
/// use datumesh_core::{Cell, Scalar};
/// use datumesh_core::Slice;
/// use datumesh_store::{Binding, Namespace, PgStore, Resolver, Result, SliceMember, SliceValue, Store, Version};
///
/// struct Wrapper(PgStore);
/// impl Resolver for Wrapper {
///     fn descriptor(&self, h: &str) -> Option<Cow<'_, [u8]>> { self.0.descriptor(h) }
///     fn receipt(&self, h: &str) -> Option<Cow<'_, [u8]>> { self.0.receipt(h) }
///     fn evaluate(&self, c: &Cell, a: &str) -> Option<Scalar> { self.0.evaluate(c, a) }
///     fn manifest(&self, e: &str) -> Option<Cow<'_, [u8]>> { self.0.manifest(e) }
/// }
/// impl Store for Wrapper {
///     fn put_descriptor(&self, b: &[u8]) -> Result<String> { self.0.put_descriptor(b) }
///     fn put_receipt(&self, b: &[u8]) -> Result<String> { self.0.put_receipt(b) }
///     fn put_blob(&self, b: &[u8]) -> Result<String> { self.0.put_blob(b) }
///     fn blob(&self, s: &str) -> Result<Option<Cow<'_, [u8]>>> { self.0.blob(s) }
///     fn put_version(&self, c: &str, t: &str, r: &str) -> Result<()> { self.0.put_version(c, t, r) }
///     fn version_asof(&self, c: &str, a: &str) -> Result<Option<Version>> { self.0.version_asof(c, a) }
///     fn versions(&self, c: &str, f: &str, t: &str) -> Result<Vec<Version>> { self.0.versions(c, f, t) }
///     fn latest(&self, c: &str) -> Result<Option<Version>> { self.0.latest(c) }
///     fn dependents(&self, r: &str) -> Result<Vec<String>> { self.0.dependents(r) }
///     fn inputs(&self, r: &str) -> Result<Vec<String>> { self.0.inputs(r) }
///     fn put_namespace(&self, h: &str, k: &[u8], v: &str) -> Result<()> { self.0.put_namespace(h, k, v) }
///     fn namespace(&self, h: &str) -> Result<Option<Namespace>> { self.0.namespace(h) }
///     fn bind_name(&self, n: &str, f: &str, t: &str) -> Result<()> { self.0.bind_name(n, f, t) }
///     fn resolve_name(&self, n: &str, a: &str) -> Result<Option<String>> { self.0.resolve_name(n, a) }
///     fn resolve_binding(&self, n: &str, a: &str) -> Result<Option<Binding>> { self.0.resolve_binding(n, a) }
///     fn put_computed(&self, c: &str, t: &str, r: &str) -> Result<()> { self.0.put_computed(c, t, r) }
///     fn computed_asof(&self, c: &str, a: &str) -> Result<Option<Version>> { self.0.computed_asof(c, a) }
///     fn stored_asof(&self, c: &str, a: &str) -> Result<Option<Version>> { self.0.stored_asof(c, a) }
///     fn cell_dependents(&self, c: &str) -> Result<Vec<String>> { self.0.cell_dependents(c) }
///     fn put_manifest(&self, b: &[u8]) -> Result<String> { self.0.put_manifest(b) }
///     fn member_manifests(&self, r: &str) -> Result<Vec<String>> { self.0.member_manifests(r) }
///     fn slice_entries(&self, r: &str) -> Result<Vec<String>> { self.0.slice_entries(r) }
///     fn slice_dependents(&self, e: &str) -> Result<Vec<String>> { self.0.slice_dependents(e) }
///     fn slice_members(&self, s: &Slice, a: &str) -> Result<Vec<SliceMember>> { self.0.slice_members(s, a) }
///     fn versions_between(&self, c: &str, f: &str, t: &str, a: &str) -> Result<Vec<SliceMember>> { self.0.versions_between(c, f, t, a) }
///     fn slice_values(&self, s: &Slice, a: &str) -> Result<Vec<SliceValue>> { self.0.slice_values(s, a) }
///     fn slice_changed_after(&self, s: &Slice, a: &str) -> Result<bool> { self.0.slice_changed_after(s, a) }
/// }
/// ```
pub trait Store: Resolver + sealed::Sealed {
    /// Stores a descriptor (checked and canonicalised by the core); returns its function
    /// hash. Idempotent.
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String>;
    /// Stores a receipt that verifies (SPEC 10.1 steps 1-6) against the key document stored
    /// for its signer host with [`put_namespace`](Store::put_namespace), in canonical form,
    /// with an edge to each of its inputs; returns its hash. An input that is a slice entry
    /// (`slice:<slice id>:<members hash>`, since 0.3.1) is recorded as such
    /// ([`slice_entries`](Store::slice_entries)); its manifest need not be stored, before or
    /// after (it is needed only to recompute, SPEC 10.1 step 7.5). The receipt's cell is
    /// indexed by its typed arguments ([`slice_members`](Store::slice_members)). A receipt that does not verify
    /// is refused with `verify`'s code (no key document for the host: `E_KEY_UNAVAILABLE`).
    /// Idempotent: bytes whose canonical form is the stored copy return its hash without
    /// checking the signature again (it verified when stored), even if the key has since
    /// been revoked. Other bytes with a stored hash are verified first, then refused as
    /// [`Error::Conflict`].
    fn put_receipt(&self, bytes: &[u8]) -> Result<String>;
    /// Stores blob content; returns its SHA-256 (lowercase hex). Idempotent.
    fn put_blob(&self, bytes: &[u8]) -> Result<String>;
    /// Blob content by SHA-256.
    fn blob(&self, sha256: &str) -> Result<Option<Cow<'_, [u8]>>>;
    /// Appends a version: from `tx` on, `cell_id` has the value of receipt `receipt_hash`,
    /// which must be stored, of that cell and at that tx. `tx` must be later than the cell's
    /// latest version ([`Error::NotLater`]).
    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()>;
    /// The cell's latest version with `tx <= asof`.
    fn version_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>>;
    /// The cell's versions with `from <= tx <= to`, in tx order.
    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Result<Vec<Version>>;
    /// The cell's latest version.
    fn latest(&self, cell_id: &str) -> Result<Option<Version>>;
    /// The stored receipts that list `receipt_hash` among their inputs, by hash. Only as
    /// an input of its own: a receipt read as a member of a slice is found through its
    /// manifests ([`member_manifests`](Store::member_manifests), then
    /// [`slice_dependents`](Store::slice_dependents)).
    fn dependents(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The inputs of a stored receipt that are receipt hashes (whether or not they are
    /// stored). Its slice entries are [`slice_entries`](Store::slice_entries).
    fn inputs(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// Records a host's key document, checked as a 200 response for that host would be
    /// (SPEC 9.2, 9.3), validated at `validated_at`. Re-validation replaces the document; a
    /// `validated_at` earlier than the recorded one is [`Error::NotLater`]. Receipts already
    /// stored are not re-checked: they verified when they were stored (and putting one again
    /// stays idempotent).
    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()>;
    /// A host's recorded key document.
    fn namespace(&self, host: &str) -> Result<Option<Namespace>>;
    /// Appends a binding: from `tx` on, the fully qualified function name `fn_name` means
    /// `fn_hash`, whose descriptor must be stored. `tx` must be later than the name's latest
    /// binding ([`Error::NotLater`]).
    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()>;
    /// The function hash `fn_name` meant as of `asof`: the hash of
    /// [`resolve_binding`](Store::resolve_binding)'s binding.
    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>>;
    /// The binding of `fn_name` that answers as of `asof`: the one with the latest `tx <=
    /// asof`, with its function hash and its `tx`, when it began; `None` before the first.
    /// A name bound again to a hash it meant before (even the hash of the binding before
    /// it) begins again: the later binding's `tx`. Since 0.3.0.
    fn resolve_binding(&self, fn_name: &str, asof: &str) -> Result<Option<Binding>>;

    /// Records that `receipt_hash` (stored, a receipt of `cell_id` at `tx`) is a computed
    /// version of the cell. Insert-only, in any order (an older version may be added after a
    /// newer one): `tx` need not be later than anything. The version's asof is the
    /// receipt's. Idempotent for the same (cell_id, tx, asof, receipt_hash); another receipt
    /// for an existing (cell_id, tx, asof) is [`Error::Conflict`] (its `hash` is the receipt
    /// refused) and the stored one is kept. The receipt is checked as for
    /// [`put_version`](Store::put_version): [`Error::NotFound`] when it is not stored,
    /// [`Error::Mismatch`] when it is of another cell or at another tx. Since 0.2.0.
    fn put_computed(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()>;
    /// The computed version that answers as of `asof`: the one with the greatest asof <=
    /// `asof` (between equal asof, the greatest tx), or `None`. Since 0.2.0.
    fn computed_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>>;
    /// The stored version that answers as of `asof`: the latest version (by tx) with `tx <=
    /// asof` whose receipt's asof is also `<= asof`, or `None`. A version whose receipt is
    /// late-signed (asof later than tx) answers only from its asof on; until then the
    /// version before it answers. ([`version_asof`](Store::version_asof) keeps its tx-only
    /// meaning.) Since 0.2.0.
    fn stored_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>>;
    /// The cells that read `cell_id`: the cell ids of the stored receipts that list, among
    /// their inputs, any stored receipt of `cell_id` (lineage: edges joined to receipts),
    /// or (since 0.3.1) a slice entry whose stored manifest lists one as a member.
    /// Ascending, each once. Since 0.2.0.
    fn cell_dependents(&self, cell_id: &str) -> Result<Vec<String>>;

    /// Stores a manifest (SPEC 15.13), `{"members": [...], "slice": {...}}`: parsed strictly
    /// and checked by the core (`Manifest::from_json`: structure, members, their order and
    /// limit, the slice object), kept in canonical form under its entry
    /// `slice:<slice id>:<members hash>`, which it returns, and recorded for each member
    /// ([`member_manifests`](Store::member_manifests)). The members need not be stored.
    /// Idempotent; insert-only (the entry is a hash of the content, so a stored entry
    /// always has these bytes). Served by [`manifest`](Resolver::manifest). Since 0.3.1.
    fn put_manifest(&self, bytes: &[u8]) -> Result<String>;
    /// The entries of the stored manifests that list `receipt_hash` among their members,
    /// ascending: from a member, the slices it was read in (then
    /// [`slice_dependents`](Store::slice_dependents)). Since 0.3.1.
    fn member_manifests(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The slice entries among the inputs of a stored receipt, ascending. Since 0.3.1.
    fn slice_entries(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The stored receipts whose inputs list the slice entry `entry`, by hash, ascending.
    /// An entry is `slice:` and two 64-digit hashes (`E_CANON_HEX` otherwise). Since 0.3.1.
    fn slice_dependents(&self, entry: &str) -> Result<Vec<String>>;
    /// The members of `slice` as of `asof` (SPEC 15.13), with the stored version of each
    /// that answers as of `asof` and its receipt's bytes.
    ///
    /// For a slice of cells: the cells of the slice's function (its `fn_hash`, `ns` and
    /// `fn`, with the same argument names) whose bound arguments are the same typed scalar,
    /// whose ranged arguments are of the range's type and within it (`lo <= v <= hi`,
    /// inclusive, in the type's order: numbers as exact decimals, dates and instants
    /// chronologically, strings by code point; `lo` after `hi` is empty), any value for an
    /// open argument, and that have a stored version answering as of `asof`: as
    /// [`stored_asof`](Store::stored_asof), the latest version with `tx <= asof` whose
    /// receipt's `asof <= asof` (so a late-signed version is not a member's answer before
    /// its asof, and a correction replaces a member's version from its asof on). Computed
    /// versions are not members. In key order: by the open and ranged arguments in
    /// ascending name order, each in its type's order (`false` before `true`; blobs as
    /// text); the cell id last, for two cells whose key arguments are of different types.
    ///
    /// For a version slice `{"cell", "from", "to"}`: [`versions_between`](Store::versions_between)
    /// of its cell. Empty when nothing matches. Since 0.3.1.
    fn slice_members(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceMember>>;
    /// The stored versions of `cell_id` with `from <= tx <= to` that answer as of `asof`
    /// (`tx <= asof` and the receipt's `asof <= asof`), ascending by tx, each with its
    /// receipt's bytes: SPEC 15.13's version slice. Empty when `from` is after `to`. Since
    /// 0.3.1.
    fn versions_between(&self, cell_id: &str, from: &str, to: &str, asof: &str) -> Result<Vec<SliceMember>>;
    /// The members [`slice_members`](Store::slice_members) gives, in its order and by its
    /// answering rule, each as its version, its receipt's typed value and its cell (with its
    /// arguments), without the receipt's bytes (the value is a stored column of the receipt
    /// and the cell comes from the typed-argument index, neither parsed per call).
    /// Since 0.3.2.
    fn slice_values(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceValue>>;
    /// Whether the members of `slice`, or their versions, may answer differently as of some
    /// time after `asof`: whether a cell matching the slice has a stored version whose
    /// receipt's `asof` is after `asof` (its `tx` is never later than that), for a version
    /// slice one with `tx` in `[from, to]`. `false` means [`slice_members`](Store::slice_members)
    /// (and `slice_values`) give the same answer as of every time from `asof` on, until a
    /// version is stored with an asof after it; `true` may be conservative (a late-signed
    /// version superseded before its asof never answers). One indexed query (receipts by
    /// function and asof), whatever the slice's size. Since 0.3.2.
    fn slice_changed_after(&self, slice: &Slice, asof: &str) -> Result<bool>;

    /// [`computed_asof`](Store::computed_asof) and the bytes of the version's receipt (as
    /// [`receipt`](Resolver::receipt) returns them), which is what a caller reads next.
    /// The default asks for the two in turn (a receipt that cannot be read then is
    /// [`Error::NotFound`]); [`PgStore`] answers both with one query. Since 0.2.0.
    fn computed_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        match self.computed_asof(cell_id, asof)? {
            Some(v) => with_receipt(self, v).map(Some),
            None => Ok(None),
        }
    }
    /// [`stored_asof`](Store::stored_asof) and the bytes of the version's receipt, as
    /// [`computed_asof_with_receipt`](Store::computed_asof_with_receipt) does. Since 0.2.0.
    fn stored_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        match self.stored_asof(cell_id, asof)? {
            Some(v) => with_receipt(self, v).map(Some),
            None => Ok(None),
        }
    }
}

/// The seal of [`Store`]: a public trait in a private module, so no other crate can name it,
/// and so none can implement `Store`.
mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::PgStore {}
}

/// A version and its receipt's bytes, for the default `*_with_receipt` methods.
fn with_receipt<S: Store + ?Sized>(store: &S, v: Version) -> Result<(Version, Cow<'_, [u8]>)> {
    match store.receipt(&v.receipt_hash) {
        Some(bytes) => Ok((v, bytes)),
        None => Err(Error::NotFound { what: "receipt", key: v.receipt_hash }),
    }
}
