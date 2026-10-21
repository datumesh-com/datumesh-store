//! A PostgreSQL store for [Datumesh](https://datumesh.com): function descriptors, receipts,
//! blobs, versions of cells over transaction time, lineage, namespaces and function names.
//! [`PgStore`] implements [`Store`], and through it `datumesh_core`'s [`Resolver`], so
//! `datumesh_core::verify` can recompute receipts of stored cells from it.
//!
//! The API is synchronous (the `postgres` driver, a small connection pool). Async callers run
//! it on a blocking thread, e.g. `tokio::task::spawn_blocking`.
//!
//! What is stored, and how:
//!
//! * Descriptors are checked by the core (`Descriptor::from_json`) and stored in canonical
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
//! * Lineage: `put_receipt` records an edge to each of the receipt's `inputs`, held or not.
//! * Namespaces: a host's key document (checked as SPEC 9.2 requires), with when it was
//!   validated. Store it before the host's receipts. Replacing it later (a key revoked or
//!   dropped) does not touch receipts already stored: they verified when they were stored,
//!   and putting one of them again is still idempotent.
//! * Names: which function hash a fully qualified function name meant from a transaction
//!   time on. Append-only, like versions.
//!
//! Instants are compared as text (fixed width, `COLLATE "C"`), which is exact; each is also
//! stored as `timestamptz`, which round-trips every instant of the specification exactly.
//!
//! Every [`Store`] method returns a `Result`: a malformed argument is [`Error::Invalid`] with
//! the core's code, a database failure [`Error::Database`]. The [`Resolver`] methods keep the
//! core's `Option` signatures; there a failure is logged with `tracing` (target
//! `datumesh_store`) and answered `None`, which `verify` reports as unavailable.
//!
//! The schema comes from [`migrate::apply`] (the `datumesh-store migrate` command).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::borrow::Cow;

pub use datumesh_core::Resolver;
pub use postgres;

mod error;
pub mod migrate;
mod pg;
mod pool;
mod sql;

pub use error::{Error, Result};
pub use pg::PgStore;
pub use pool::PoolOptions;

/// A version of a cell: from transaction time `tx` on, the cell's value is that of the
/// receipt `receipt_hash`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    /// The cell id.
    pub cell_id: String,
    /// The transaction time (an instant, SPEC 3.3).
    pub tx: String,
    /// The receipt stating the value.
    pub receipt_hash: String,
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
pub trait Store: Resolver {
    /// Stores a descriptor (checked and canonicalised by the core); returns its function
    /// hash. Idempotent.
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String>;
    /// Stores a receipt that verifies (SPEC 10.1 steps 1-6) against the key document stored
    /// for its signer host with [`put_namespace`](Store::put_namespace), in canonical form,
    /// with an edge to each of its inputs; returns its hash. A receipt that does not verify
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
    /// The stored receipts that list `receipt_hash` among their inputs, by hash.
    fn dependents(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The inputs of a stored receipt, by hash (whether or not they are stored).
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
    /// The function hash `fn_name` meant as of `asof`.
    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>>;
}
