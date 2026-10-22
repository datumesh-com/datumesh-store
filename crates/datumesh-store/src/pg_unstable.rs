//! Outside semver until 1.0: this module exposes the `postgres` driver's types, and any
//! release may change it, including a patch release.
//!
//! For what [`PgStore::connect_with`] does not cover: a store whose connections come from
//! any function returning a `postgres::Client` (client certificates, a custom TLS setup,
//! session settings), and plain clients that connect exactly as the store does (the same
//! TLS connector and roots). [`postgres`] is the driver this crate is built with, so a
//! `Client` made with it is the type [`with_connector`] takes. Depend on an exact version
//! of this crate (`datumesh-store = "=x.y.z"`) if you use this module.

use std::fmt;

use tokio_postgres_rustls::MakeRustlsConnect;

pub use postgres;

use crate::error::Result;
use crate::pool::{Pool, PoolOptions};
use crate::PgStore;

/// A store whose pool opens connections with `connect` (any TLS setup, any configuration).
/// One connection is opened at once, so a failing connector fails here.
///
/// Outside semver until 1.0: this function names the `postgres` driver's types, and any
/// release may change it, including a patch release.
pub fn with_connector<F>(connect: F, options: PoolOptions) -> Result<PgStore>
where
    F: Fn() -> core::result::Result<postgres::Client, postgres::Error> + Send + Sync + 'static,
{
    PgStore::from_pool(Pool::new(Box::new(connect), options))
}

/// Opens `postgres` clients the way [`PgStore::connect_with`] does: the configuration's
/// `sslmode` decides whether TLS is used, and when it is, the server's certificate and host
/// name are verified against the Mozilla roots and the roots of the [`PoolOptions`]
/// (rustls, ring provider). Cheap to clone.
///
/// Outside semver until 1.0: this type names the `postgres` driver's types, and any release
/// may change it, including a patch release.
#[derive(Clone)]
pub struct Connector {
    config: postgres::Config,
    tls: MakeRustlsConnect,
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector").field("config", &self.config).finish_non_exhaustive()
    }
}

impl Connector {
    /// A connector for `config`, with the TLS roots of `options` (its pool settings are not
    /// used). Fails with [`Error::Config`](crate::Error::Config) if a root does not parse.
    pub fn new(config: postgres::Config, options: &PoolOptions) -> Result<Connector> {
        Ok(Connector { config, tls: crate::tls::connector(options)? })
    }

    /// Opens a client.
    pub fn connect(&self) -> core::result::Result<postgres::Client, postgres::Error> {
        self.config.connect(self.tls.clone())
    }
}
