//! A small connection pool: no global state, no background threads, and a per-connection
//! cache of prepared statements (each statement is prepared once per connection, on first
//! use).

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use postgres::{Client, Statement};

use crate::error::{Error, Result};
use crate::sql::Q;

/// Opens a new connection.
pub(crate) type Connector = Box<dyn Fn() -> core::result::Result<Client, postgres::Error> + Send + Sync>;

/// How the pool behaves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolOptions {
    max_size: usize,
    checkout_timeout: Duration,
}

impl Default for PoolOptions {
    fn default() -> PoolOptions {
        PoolOptions::new()
    }
}

impl PoolOptions {
    /// At most 8 connections; wait at most 30 seconds for one.
    pub fn new() -> PoolOptions {
        PoolOptions { max_size: 8, checkout_timeout: Duration::from_secs(30) }
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
}

/// A connection and the statements prepared on it.
pub(crate) struct Conn {
    pub(crate) client: Client,
    statements: HashMap<Q, Statement>,
}

impl Conn {
    /// The prepared statement for `q`, preparing it on first use.
    pub(crate) fn stmt(&mut self, q: Q) -> core::result::Result<Statement, postgres::Error> {
        if let Some(s) = self.statements.get(&q) {
            return Ok(s.clone());
        }
        let s = self.client.prepare_typed(q.sql(), q.types())?;
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
        let mut client = (self.connect)()?;
        client.batch_execute(SESSION)?;
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
