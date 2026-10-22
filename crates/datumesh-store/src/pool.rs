//! A small connection pool: no global state, no background threads, and a per-connection
//! cache of prepared statements (each statement is prepared once per connection, on first
//! use, never when connecting).

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use postgres::{Client, Statement};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;
use rustls::RootCertStore;

use crate::error::{Db, Error, Result};
use crate::sql::Q;

/// Opens a new connection.
pub(crate) type Connector = Box<dyn Fn() -> core::result::Result<Client, postgres::Error> + Send + Sync>;

/// How the pool behaves, and the roots TLS connections are verified against.
///
/// Builder style: `PoolOptions::new().max_size(4).tls_root_certificates_pem(&pem)?`.
#[derive(Clone, PartialEq, Eq)]
pub struct PoolOptions {
    max_size: usize,
    checkout_timeout: Duration,
    pub(crate) webpki_roots: bool,
    pub(crate) tls_roots: Vec<CertificateDer<'static>>,
}

impl std::fmt::Debug for PoolOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolOptions")
            .field("max_size", &self.max_size)
            .field("checkout_timeout", &self.checkout_timeout)
            .field("webpki_roots", &self.webpki_roots)
            .field("tls_root_certificates", &self.tls_roots.len())
            .finish()
    }
}

impl Default for PoolOptions {
    fn default() -> PoolOptions {
        PoolOptions::new()
    }
}

impl PoolOptions {
    /// At most 8 connections; wait at most 30 seconds for one; TLS connections verified
    /// against the Mozilla roots (`webpki-roots`).
    pub fn new() -> PoolOptions {
        PoolOptions {
            max_size: 8,
            checkout_timeout: Duration::from_secs(30),
            webpki_roots: true,
            tls_roots: Vec::new(),
        }
    }

    /// The most connections open at once (at least 1).
    pub fn max_size(mut self, n: usize) -> PoolOptions {
        self.max_size = n.max(1);
        self
    }

    /// How long a call waits for a free connection before failing with
    /// [`Error::PoolTimeout`] (reads: `None`).
    pub fn checkout_timeout(mut self, d: Duration) -> PoolOptions {
        self.checkout_timeout = d;
        self
    }

    /// Adds the certificates of a PEM bundle (one or more `CERTIFICATE` sections; other
    /// sections are ignored) to the roots TLS connections are verified against: a private
    /// CA, or a server's self-signed certificate. Calls append. A bundle with no
    /// certificate, or with one that does not parse, is [`Error::Config`].
    pub fn tls_root_certificates_pem(mut self, pem: &[u8]) -> Result<PoolOptions> {
        let mut check = RootCertStore::empty();
        let mut n = 0;
        for cert in CertificateDer::pem_slice_iter(pem) {
            n += 1;
            let cert = cert.map_err(|e| Error::Config(format!("root certificate {n} of the PEM bundle: {e}")))?;
            check
                .add(cert.clone())
                .map_err(|e| Error::Config(format!("root certificate {n} of the PEM bundle: {e}")))?;
            self.tls_roots.push(cert);
        }
        if n == 0 {
            return Err(Error::Config("the PEM bundle holds no certificate".into()));
        }
        Ok(self)
    }

    /// Whether the Mozilla roots (`webpki-roots`, compiled in) are trusted too: yes by
    /// default. `false` trusts only the roots added with
    /// [`tls_root_certificates_pem`](Self::tls_root_certificates_pem) (a private CA).
    pub fn tls_webpki_roots(mut self, yes: bool) -> PoolOptions {
        self.webpki_roots = yes;
        self
    }
}

/// A connection and the statements prepared on it.
pub(crate) struct Conn {
    pub(crate) client: Client,
    statements: HashMap<Q, Statement>,
}

impl Conn {
    /// The prepared statement for `q`, preparing it on first use.
    pub(crate) fn stmt(&mut self, q: Q) -> Result<Statement> {
        if let Some(s) = self.statements.get(&q) {
            return Ok(s.clone());
        }
        let s = self.client.prepare_typed(q.sql(), q.types()).db()?;
        self.statements.insert(q, s.clone());
        Ok(s)
    }
}

struct State {
    idle: Vec<Conn>,
    open: usize,
}

pub(crate) struct Pool {
    connect: Connector,
    state: Mutex<State>,
    freed: Condvar,
    options: PoolOptions,
}

/// Settings every connection gets: the append-only triggers rely on READ COMMITTED.
const SESSION: &str = "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL READ COMMITTED; \
                       SET TIME ZONE 'UTC'; SET DateStyle = 'ISO, YMD'";

impl Pool {
    pub(crate) fn new(connect: Connector, options: PoolOptions) -> Pool {
        Pool { connect, state: Mutex::new(State { idle: Vec::new(), open: 0 }), freed: Condvar::new(), options }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock cannot leave State inconsistent (two fields, each
        // updated in one step), so a poisoned lock is still usable.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A free connection, a new one if fewer than `max_size` are open, or one freed within
    /// the checkout timeout.
    pub(crate) fn get(&self) -> Result<Pooled<'_>> {
        let deadline = Instant::now() + self.options.checkout_timeout;
        let mut state = self.lock();
        loop {
            while let Some(conn) = state.idle.pop() {
                if conn.client.is_closed() {
                    state.open -= 1;
                    continue;
                }
                return Ok(Pooled { pool: self, conn: Some(conn) });
            }
            if state.open < self.options.max_size {
                state.open += 1;
                drop(state);
                return match self.open() {
                    Ok(conn) => Ok(Pooled { pool: self, conn: Some(conn) }),
                    Err(e) => {
                        self.lock().open -= 1;
                        self.freed.notify_one();
                        Err(e)
                    }
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::PoolTimeout);
            }
            state = self.freed.wait_timeout(state, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn open(&self) -> Result<Conn> {
        let mut client = (self.connect)().db()?;
        client.batch_execute(SESSION).db()?;
        Ok(Conn { client, statements: HashMap::new() })
    }

    fn put_back(&self, conn: Conn) {
        let mut state = self.lock();
        if conn.client.is_closed() {
            state.open -= 1;
        } else {
            state.idle.push(conn);
        }
        drop(state);
        self.freed.notify_one();
    }
}

/// A connection checked out of the pool; returned to it when dropped (or discarded if the
/// server closed it).
pub(crate) struct Pooled<'a> {
    pool: &'a Pool,
    conn: Option<Conn>,
}

impl Deref for Pooled<'_> {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        // Some from creation until drop.
        self.conn.as_ref().expect("connection present until drop")
    }
}

impl DerefMut for Pooled<'_> {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("connection present until drop")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.pool.put_back(conn);
        }
    }
}
