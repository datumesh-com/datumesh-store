//! The store's error type.

use std::fmt;

use datumesh_core::ErrorCode;

/// What a [`Store`](crate::Store) method or a migration can fail with.
#[non_exhaustive]
#[derive(Debug)]
pub enum Error {
    /// The core rejected the input: not canonical, not a receipt, a receipt that does not
    /// verify against the stored key document, not an instant or hash, a key document that
    /// does not satisfy SPEC 9.2, ... The core's error carries its code
    /// ([`Error::code`]), JSON path and detail.
    Invalid(datumesh_core::Error),
    /// An append-only history refused a time that is not later than its latest entry:
    /// `put_version` (`what` = `"version"`, `key` = the cell id), `bind_name` (`"name"`,
    /// the function name) or `put_namespace` (`"namespace"`, the host; equal is allowed
    /// there).
    NotLater {
        /// `"version"`, `"name"` or `"namespace"`.
        what: &'static str,
        /// The cell id, function name or host.
        key: String,
        /// The time given.
        given: String,
        /// The latest time already recorded.
        latest: String,
    },
    /// Something the write refers to is not in the store: the receipt of a version, the
    /// descriptor of a name binding.
    NotFound {
        /// `"receipt"` or `"descriptor"`.
        what: &'static str,
        /// Its hash.
        key: String,
    },
    /// The receipt named by `put_version` is for another cell, or has another `tx`.
    Mismatch {
        /// `"cell_id"` or `"tx"`.
        what: &'static str,
        /// The value given to `put_version`.
        expected: String,
        /// The receipt's value.
        found: String,
    },
    /// A receipt with this hash is stored with other bytes: the same payload with another
    /// valid signature by the same key (the receipt hash does not cover `sig`; it covers
    /// `computed_by`, so a rotated key gives another hash). Only a randomised signer makes
    /// two valid signatures; RFC 8032 Ed25519 is deterministic. The stored receipt is kept.
    Conflict {
        /// The receipt hash.
        hash: String,
    },
    /// The database refused or failed.
    Database(postgres::Error),
    /// No connection became free within the pool's checkout timeout.
    PoolTimeout,
    /// A migration could not be applied or checked.
    Migration(String),
}

impl Error {
    /// The core's error code, for [`Error::Invalid`].
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Error::Invalid(e) => Some(e.code()),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(e) => write!(f, "invalid input: {e}"),
            Error::NotLater { what, key, given, latest } => {
                write!(f, "{what} {key}: {given} is not later than the latest, {latest}")
            }
            Error::NotFound { what, key } => write!(f, "no {what} {key} in the store"),
            Error::Mismatch { what, expected, found } => {
                write!(f, "the receipt's {what} is {found}, not {expected}")
            }
            Error::Conflict { hash } => {
                write!(f, "receipt {hash} is stored with another valid signature; kept the stored one")
            }
            Error::Database(e) => match e.as_db_error() {
                Some(d) => write!(f, "database: {} ({})", d.message(), d.code().code()),
                None => write!(f, "database: {e}"),
            },
            Error::PoolTimeout => f.write_str("no database connection became free in time"),
            Error::Migration(m) => write!(f, "migration: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Invalid(e) => Some(e),
            Error::Database(e) => Some(e),
            _ => None,
        }
    }
}

impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Error {
        Error::Database(e)
    }
}

impl From<datumesh_core::Error> for Error {
    fn from(e: datumesh_core::Error) -> Error {
        Error::Invalid(e)
    }
}

/// `Result` with the store's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
