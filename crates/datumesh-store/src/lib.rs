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
//! * Descriptors and receipts are checked by the core (`Descriptor::from_json`,
//!   `Receipt::from_json`: SPEC 10.1 steps 1-4) and stored in canonical form (JCS), under
//!   their hash. Storing the same thing twice stores it once. A receipt with the hash of a
//!   stored one but another signature is refused ([`Error::Conflict`]): the store does not
//!   verify signatures, so it does not replace one.
//! * Blobs are stored in the database, under the SHA-256 of their content.
//! * Versions: from transaction time `tx` on, cell `cell_id` has the value of a receipt.
//!   Append-only and in tx order per cell, enforced by the database (triggers) as well as by
//!   [`Store::put_version`], which also checks that the receipt is stored and is of that
//!   cell at that tx.
//! * Lineage: `put_receipt` records an edge to each of the receipt's `inputs`, held or not.
//! * Namespaces: a host's key document (checked as SPEC 9.2 requires), with when it was
//!   validated.
//! * Names: which function hash a fully qualified function name meant from a transaction
//!   time on. Append-only, like versions.
//!
//! Instants are compared as text (fixed width, `COLLATE "C"`), which is exact; each is also
//! stored as `timestamptz`, which round-trips every instant of the specification exactly.
//!
//! Reads return `Option` / `Vec`, as the [`Store`] signatures require. A malformed argument
//! gives `None` / empty without a query; a database error is logged with `tracing` (target
//! `datumesh_store`) and also gives `None` / empty, so **the read signatures do not let a
//! caller tell "not stored" from "the database failed"**.
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
/// Instants are SPEC 3.3 instants (`YYYY-MM-DDTHH:MM:SS.ffffffZ`) and hashes 64 lowercase hex
/// digits; the write methods refuse anything else with the core's error code
/// ([`Error::Invalid`]), the read methods answer it with `None` / empty.
pub trait Store: Resolver {
    /// Stores a descriptor (checked and canonicalised by the core); returns its function
    /// hash. Idempotent.
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String>;
    /// Stores a receipt (steps 1-4 of SPEC 10.1, canonical form) and an edge to each of its
    /// inputs; returns its hash. Idempotent; the same hash with other bytes is
    /// [`Error::Conflict`].
    fn put_receipt(&self, bytes: &[u8]) -> Result<String>;
    /// Stores blob content; returns its SHA-256 (lowercase hex). Idempotent.
    fn put_blob(&self, bytes: &[u8]) -> Result<String>;
    /// Blob content by SHA-256.
    fn blob(&self, sha256: &str) -> Option<Cow<'_, [u8]>>;
    /// Appends a version: from `tx` on, `cell_id` has the value of receipt `receipt_hash`,
    /// which must be stored, of that cell and at that tx. `tx` must be later than the cell's
    /// latest version ([`Error::NotLater`]).
    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()>;
    /// The cell's latest version with `tx <= asof`.
    fn version_asof(&self, cell_id: &str, asof: &str) -> Option<Version>;
    /// The cell's versions with `from <= tx <= to`, in tx order.
    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Vec<Version>;
    /// The cell's latest version.
    fn latest(&self, cell_id: &str) -> Option<Version>;
    /// The stored receipts that list `receipt_hash` among their inputs, by hash.
    fn dependents(&self, receipt_hash: &str) -> Vec<String>;
    /// The inputs of a stored receipt, by hash (whether or not they are stored).
    fn inputs(&self, receipt_hash: &str) -> Vec<String>;
    /// Records a host's key document, checked as a 200 response for that host would be
    /// (SPEC 9.2, 9.3), validated at `validated_at`. Re-validation replaces the document; a
    /// `validated_at` earlier than the recorded one is [`Error::NotLater`].
    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()>;
    /// A host's recorded key document.
    fn namespace(&self, host: &str) -> Option<Namespace>;
    /// Appends a binding: from `tx` on, the fully qualified function name `fn_name` means
    /// `fn_hash`, whose descriptor must be stored. `tx` must be later than the name's latest
    /// binding ([`Error::NotLater`]).
    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()>;
    /// The function hash `fn_name` meant as of `asof`.
    fn resolve_name(&self, fn_name: &str, asof: &str) -> Option<String>;
}
