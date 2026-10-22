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
    /// The database refused or failed: the server's message and SQLSTATE, or the driver's
    /// description of a failure that is not the server's (no connection, a URL that does
    /// not parse, a TLS handshake refused, a certificate that did not verify).
    Database(DatabaseError),
    /// No connection became free within the pool's checkout timeout.
    PoolTimeout,
    /// A migration could not be applied or checked.
    Migration(String),
    /// A connection option is unusable: a root certificate bundle
    /// ([`PoolOptions::tls_root_certificates_pem`](crate::PoolOptions::tls_root_certificates_pem))
    /// with no certificate, or one that does not parse.
    Config(String),
}

/// A database failure, [`Error::Database`]: what the server said (its message and
/// SQLSTATE), or the driver's description of a failure that is not the server's.
///
/// Its [`source`](std::error::Error::source) is the driver's error (type erased: the
/// driver's types are not part of this crate's API outside
/// [`pg_unstable`](crate::pg_unstable)).
#[derive(Debug)]
pub struct DatabaseError {
    message: String,
    code: Option<String>,
    source: postgres::Error,
}

impl DatabaseError {
    pub(crate) fn new(e: postgres::Error) -> DatabaseError {
        let (message, code) = match e.as_db_error() {
            Some(d) => (d.message().to_string(), Some(d.code().code().to_string())),
            None => (driver_text(&e), None),
        };
        DatabaseError { message, code, source: e }
    }

    /// The server's message (`relation "versions" does not exist`), or the driver's text
    /// and its causes when the server said nothing (`error connecting to server: ...`,
    /// `error performing TLS handshake: invalid peer certificate: ...`).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The SQLSTATE the server reported (five characters, e.g. `42P01`), if it was the
    /// server's error.
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }
}

/// The driver's error and its sources (`error performing TLS handshake: invalid peer
/// certificate: UnknownIssuer`): the driver's own text often omits the cause.
fn driver_text(e: &postgres::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        let s_text = s.to_string();
        if !text.contains(&s_text) {
            text.push_str(": ");
            text.push_str(&s_text);
        }
        source = s.source();
    }
    text
}

impl fmt::Display for DatabaseError {
    /// The message, then the SQLSTATE in parentheses when there is one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(c) => write!(f, "{} ({c})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DatabaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
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
            Error::Database(e) => write!(f, "database: {e}"),
            Error::PoolTimeout => f.write_str("no database connection became free in time"),
            Error::Migration(m) => write!(f, "migration: {m}"),
            Error::Config(m) => write!(f, "configuration: {m}"),
        }
    }
}

impl std::error::Error for Error {
    /// For [`Error::Database`], the driver's error (as in 0.1.0; the [`DatabaseError`] is
    /// this error's own text, so a chain of sources does not repeat it).
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Invalid(e) => Some(e),
            Error::Database(e) => std::error::Error::source(e),
            _ => None,
        }
    }
}

/// `?` on the driver's results, inside the crate: `.db()?` makes an [`Error::Database`].
pub(crate) trait Db<T> {
    fn db(self) -> Result<T>;
}

impl<T> Db<T> for core::result::Result<T, postgres::Error> {
    fn db(self) -> Result<T> {
        self.map_err(|e| Error::Database(DatabaseError::new(e)))
    }
}

impl From<datumesh_core::Error> for Error {
    fn from(e: datumesh_core::Error) -> Error {
        Error::Invalid(e)
    }
}

/// `Result` with the store's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
