//! `SqliteStore`: the store in one SQLite file (since 0.4.0).

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::catalogue::{
    aliases_json, args_json, as_i64, canonical_examples, check_catalog_entry, check_catalog_ext, check_key_values,
    entry_ext, entry_from_columns, invalid, miss_text, parse_aliases, parse_args, parse_facets, rank, CatalogRow,
    Words, SUMMARY_VALUES,
};
use crate::error::{Db, Error, Result};
use crate::listings::{
    check_listed_host, check_listing, listing_facets_json, parse_subjects, same_listing, subjects_json,
};
use crate::slices::{check_entry, cmp_keys, is_hash, is_instant, version_slice, Constraint, Kind, Pattern};
use crate::sqlite_sql::L;
use crate::{
    later_than, not_canonical_class, parse_receipt, stored_value, unbound, Binding, CatalogEntry, CatalogEntryExt,
    CatalogKind, ClassBinding, Dictionary, DictionaryValue, Embedding, FacetedEntry, FunctionTest, Hit, KeyValue,
    MemberSet, Namespace, PendingEmbedding, SliceMember, SliceValue, Store, Summary, SummaryEntry, TestSummary,
    Version, FIND_MISSES_CAP, WITHDRAWN_VALUE,
};
use crate::{ListedHost, Listing, ListingWrite};
use datumesh_core::json::{self, Value};
use datumesh_core::lexical::{check_dns_name, check_function_name, check_hex, check_instant};
use datumesh_core::{
    keys_from_response, sha256_hex, verify, Arg, Args, Cell, ClassDescriptor, ErrorCode, FunctionDescriptor, Keys,
    Manifest, Resolver, Scalar, Slice, Type,
};

/// How a [`SqliteStore`] opens and uses its connections.
///
/// Builder style: `SqliteOptions::new().busy_timeout(Duration::from_secs(5))`.
#[derive(Clone, PartialEq, Eq)]
pub struct SqliteOptions {
    max_size: usize,
    busy_timeout: Duration,
    durable: bool,
    pub(crate) recording_clock: Option<crate::pool::ClockRef>,
    /// How long the barrier waits for a write transaction in flight (decided 27 Nov, B2/B3).
    pub(crate) barrier_timeout: Duration,
}

impl std::fmt::Debug for SqliteOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteOptions")
            .field("max_size", &self.max_size)
            .field("busy_timeout", &self.busy_timeout)
            .field("barrier_timeout", &self.barrier_timeout)
            .field("durable", &self.durable)
            .field("recording_clock", &self.recording_clock.is_some())
            .finish()
    }
}

impl Default for SqliteOptions {
    fn default() -> SqliteOptions {
        SqliteOptions::new()
    }
}

impl SqliteOptions {
    /// At most 8 idle connections kept; a writer waits at most 30 seconds for the
    /// database's write lock (and a reader for a lock held in exclusive mode); every commit
    /// durable (`synchronous = FULL`).
    pub fn new() -> SqliteOptions {
        SqliteOptions {
            max_size: 8,
            busy_timeout: Duration::from_secs(30),
            durable: true,
            recording_clock: None,
            barrier_timeout: Duration::from_secs(5),
        }
    }

    /// How long the barrier waits for a write transaction in flight, in this process or another, before it
    /// fails with [`Error::Timeout`](crate::Error::Timeout). Default 5 s. Since 0.6.5.
    pub fn barrier_timeout(mut self, d: Duration) -> SqliteOptions {
        self.barrier_timeout = d;
        self
    }

    /// The clock the store records its versions at (G9, [`RecordingClock`](crate::RecordingClock)): read at each insert,
    /// `recorded = max(signed tx, clock())`. Without one a version is recorded at its signed tx.
    /// Since 0.6.5.
    pub fn recording_clock(mut self, clock: crate::RecordingClock) -> SqliteOptions {
        self.recording_clock = Some(crate::pool::ClockRef(clock));
        self
    }

    /// The most idle connections kept open for reuse (at least 1). More may be open at
    /// once, one per concurrent call; SQLite connections are cheap, and readers do not
    /// wait for each other or for the writer (WAL).
    pub fn max_size(mut self, n: usize) -> SqliteOptions {
        self.max_size = n.max(1);
        self
    }

    /// How long a call waits for a lock another connection (of this process or another)
    /// holds before failing with [`Error::Database`] (`SQLITE_BUSY`; reads through the
    /// [`Resolver`]: `None`). Writers take the database's one write lock for the duration
    /// of a `put_*`, so this is the longest a writer waits behind the others.
    pub fn busy_timeout(mut self, d: Duration) -> SqliteOptions {
        self.busy_timeout = d;
        self
    }

    /// Whether each commit is flushed to disk before it returns (`synchronous = FULL`, the
    /// default), or only at checkpoints (`synchronous = NORMAL`: a power loss can lose the
    /// latest commits, never consistency; much faster for bulk loads, like PostgreSQL's
    /// `synchronous_commit = off`). Either way an application crash loses nothing
    /// committed.
    pub fn durable(mut self, yes: bool) -> SqliteOptions {
        self.durable = yes;
        self
    }
}

/// The store in one SQLite file (bundled SQLite, no server), with the same behaviour as
/// [`PgStore`](crate::PgStore): every method, error and rule of [`Store`] and
/// [`Resolver`]. `Send + Sync`: share one per process (behind an `Arc`); each call takes a
/// connection for its duration. Since 0.4.0.
///
/// The file is in WAL mode (readers never wait for the writer), with a busy timeout
/// ([`SqliteOptions::busy_timeout`]). Every `put_*` is one transaction begun with `BEGIN
/// IMMEDIATE`: it takes the database's write lock first, so its checks (a version later
/// than the latest, a key not yet taken) and its inserts are one atomic step against every
/// other writer, threads of this process or other processes on the same machine. The
/// append-only rules are also triggers in the file, so they hold for any connection. (A
/// SQLite file on a network file system is not supported: its locks are not reliable.)
///
/// Every `put_*` is one transaction. [`Store`] methods return errors: a malformed argument
/// is [`Error::Invalid`] (no query is made), a database failure [`Error::Database`]. The
/// [`Resolver`] methods answer both with `None`; a database failure is logged with
/// `tracing` (target `datumesh_store`, level `error`).
pub struct SqliteStore {
    path: PathBuf,
    options: SqliteOptions,
    idle: Mutex<Vec<Connection>>,
    /// The store's own writers queue here before taking the database's write lock: SQLite's
    /// busy handler polls with sleeps of up to 100 ms, so threads contending for the lock
    /// would mostly sleep. Other stores and processes still meet at the database's lock.
    writer: Mutex<()>,
    /// Opened with `open_existing`: never creates the file, never changes its journal mode.
    existing: bool,
    /// Set once the versions table is found to have the recorded column (migration 13): cached (review item 6).
    recorded_schema: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore").field("path", &self.path).finish_non_exhaustive()
    }
}

/// A connection taken from the store, returned to it when dropped.
struct Pooled<'a> {
    store: &'a SqliteStore,
    conn: Option<Connection>,
}

impl std::ops::Deref for Pooled<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("a pooled connection")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            // A connection left inside a transaction (a failed rollback) is not reused.
            if c.is_autocommit() {
                if let Ok(mut idle) = self.store.idle.lock() {
                    if idle.len() < self.store.options.max_size {
                        idle.push(c);
                    }
                }
            }
        }
    }
}

impl SqliteStore {
    /// Opens the SQLite file at `path` with the default [`SqliteOptions`]; a missing file
    /// is created (empty: apply [`migrate::sqlite::apply`](crate::migrate::sqlite::apply)
    /// next), as a missing directory is not. [`open_existing`](Self::open_existing) creates
    /// nothing.
    pub fn open(path: impl AsRef<Path>) -> Result<SqliteStore> {
        SqliteStore::open_with(path, SqliteOptions::new())
    }

    /// Opens the SQLite file at `path`, creating it when it is missing (its directory must
    /// exist). One connection is opened at once (and
    /// the file put in WAL mode), so a path that cannot be opened fails here
    /// (`SQLITE_CANTOPEN`); the schema is not looked at, so a new or empty file is fine
    /// (create the schema with [`migrate::sqlite::apply`](crate::migrate::sqlite::apply)).
    pub fn open_with(path: impl AsRef<Path>, options: SqliteOptions) -> Result<SqliteStore> {
        let store = SqliteStore {
            path: path.as_ref().to_path_buf(),
            options,
            idle: Mutex::new(Vec::new()),
            writer: Mutex::new(()),
            existing: false,
            recorded_schema: std::sync::atomic::AtomicBool::new(false),
        };
        let c = store.open_connection()?;
        if let Ok(mut idle) = store.idle.lock() {
            idle.push(c);
        }
        Ok(store)
    }

    /// Opens an existing SQLite file without changing anything: a path with no file is
    /// [`Error::Database`] (`no such database file: <path>`, code `SQLITE_CANTOPEN`) and
    /// nothing is created; the file's journal mode is left as it is. For checking a
    /// database ([`migrate::sqlite::check`](crate::migrate::sqlite::check), the command's
    /// `migrate --check`), which has no side effects. Writes through such a store work
    /// as through any other. Since 0.4.0.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<SqliteStore> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(Error::Database(crate::error::DatabaseError::no_such_file(path)));
        }
        let store = SqliteStore {
            path: path.to_path_buf(),
            options: SqliteOptions::new(),
            idle: Mutex::new(Vec::new()),
            writer: Mutex::new(()),
            existing: true,
            recorded_schema: std::sync::atomic::AtomicBool::new(false),
        };
        let c = store.open_connection()?;
        if let Ok(mut idle) = store.idle.lock() {
            idle.push(c);
        }
        Ok(store)
    }

    /// Opens a store named by a URL: `sqlite:<path>` or `sqlite://<path>` (so
    /// `sqlite:///var/lib/datumesh/store.db` is an absolute path), or a plain file path.
    /// Query parameters are not taken: the path is everything after the scheme.
    pub fn connect(url: &str) -> Result<SqliteStore> {
        SqliteStore::open(sqlite_path(url))
    }

    /// The database file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn open_connection(&self) -> Result<Connection> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if !self.existing {
            flags |= OpenFlags::SQLITE_OPEN_CREATE;
        }
        let c = Connection::open_with_flags(&self.path, flags).db()?;
        register_text_functions(&c)?;
        c.busy_timeout(self.options.busy_timeout).db()?;
        c.set_prepared_statement_cache_capacity(L::ALL.len() + 16);
        if self.existing {
            // Nothing about the file changes: no journal mode, no synchronous setting.
            c.execute_batch("PRAGMA foreign_keys = ON").db()?;
            return Ok(c);
        }
        // WAL: readers and the writer do not block each other, and a commit appends to the
        // log. Changing the mode needs a lock the busy handler does not always wait for (a
        // new file opened by several processes at once), so it is retried here.
        let start = Instant::now();
        loop {
            match c.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0)) {
                Ok(mode) if mode.eq_ignore_ascii_case("wal") => break,
                Ok(mode) => return Err(Error::Config(format!("SQLite: journal mode WAL refused (it is {mode})"))),
                Err(e) if is_busy(&e) && start.elapsed() < self.options.busy_timeout => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e).db(),
            }
        }
        let sync = if self.options.durable { "FULL" } else { "NORMAL" };
        c.execute_batch(&format!("PRAGMA foreign_keys = ON; PRAGMA synchronous = {sync};")).db()?;
        Ok(c)
    }

    /// A connection: an idle one, or a new one.
    fn conn(&self) -> Result<Pooled<'_>> {
        let idle = self.idle.lock().ok().and_then(|mut v| v.pop());
        let conn = match idle {
            Some(c) => c,
            None => self.open_connection()?,
        };
        Ok(Pooled { store: self, conn: Some(conn) })
    }

    /// Prepares every SQL statement the store uses against the live schema, on a new
    /// connection, and returns how many there are. Fails with the first statement the
    /// schema cannot serve.
    pub fn check_statements(&self) -> Result<usize> {
        let c = self.open_connection()?;
        for q in L::ALL {
            c.prepare(q.sql()).db()?;
        }
        Ok(L::ALL.len())
    }

    /// A connection held for a migration (one transaction on it), or a catalog read.
    pub(crate) fn raw(&self) -> Result<impl std::ops::Deref<Target = Connection> + '_> {
        self.conn()
    }

    /// A [`Resolver`] lookup: a failure (logged by `run`) becomes `None`.
    fn resolve<T>(&self, op: &'static str, f: impl FnOnce(&Connection) -> Result<Option<T>>) -> Option<T> {
        self.run(op, f).ok().flatten()
    }

    /// Runs `f` on a connection; a database failure is logged.
    fn run<T>(&self, op: &'static str, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let result = self.conn().and_then(|c| f(&c));
        if let Err(e @ Error::Database(_)) = &result {
            tracing::error!(target: "datumesh_store", op, error = %e, "database failure");
        }
        result
    }

    /// Runs `f` in a write transaction (`BEGIN IMMEDIATE`: the write lock first), committed
    /// when `f` succeeds and rolled back otherwise; a database failure is logged.
    fn write<T>(&self, op: &'static str, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.run(op, |c| {
            let v = self.locked_write(c, f)?;
            self.bound_wal(c);
            Ok(v)
        })
    }

    /// `write`, after the store's other writers.
    fn locked_write<T>(&self, c: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let _queue = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        write(c, f)
    }

    /// Keeps the write-ahead log bounded. SQLite's automatic checkpoints copy the log into
    /// the database but restart the log only when no reader is using it, which under a
    /// steady load of concurrent readers may be never: the log then grows with every
    /// commit. After a commit, when the log has outgrown `WAL_LIMIT`, this connection runs
    /// a TRUNCATE checkpoint, which waits (the busy timeout) for the readers to move to the
    /// latest snapshot, copies everything and empties the log. A failure (still busy) is
    /// left to the next commit.
    fn bound_wal(&self, c: &Connection) {
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        if std::fs::metadata(&wal).is_ok_and(|m| m.len() > WAL_LIMIT) {
            if let Err(e) = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
                tracing::debug!(target: "datumesh_store", error = %e, "checkpoint deferred");
            }
        }
    }

    /// A version from a query answering (tx, receipt_hash, asof) for (cell_id, asof).
    fn asof_version(&self, op: &'static str, q: L, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(asof, "asof")?;
        self.run(op, |c| {
            c.prepare_cached(q.sql())
                .db()?
                .query_row(rusqlite::params![cell_id, asof], |r| version_row(r, cell_id))
                .optional()
                .db()
        })
    }

    /// A version and its receipt's bytes from a query answering (tx, receipt_hash, asof,
    /// bytes) for (cell_id, asof).
    fn asof_with_receipt(
        &self,
        op: &'static str,
        q: L,
        cell_id: &str,
        asof: &str,
    ) -> Result<Option<(Version, Vec<u8>)>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(asof, "asof")?;
        self.run(op, |c| {
            c.prepare_cached(q.sql())
                .db()?
                .query_row(rusqlite::params![cell_id, asof], |r| Ok((version_row(r, cell_id)?, r.get(5)?)))
                .optional()
                .db()
        })
    }
}

/// The size of the write-ahead log beyond which a commit checkpoints it and truncates it.
const WAL_LIMIT: u64 = 64 << 20;

/// Runs `f` in a `BEGIN IMMEDIATE` transaction on `c`.
pub(crate) fn write<T>(c: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    c.execute_batch("BEGIN IMMEDIATE").db()?;
    let result = f(c).and_then(|v| c.execute_batch("COMMIT").db().map(|()| v));
    if result.is_err() && !c.is_autocommit() {
        let _ = c.execute_batch("ROLLBACK");
    }
    result
}

impl SqliteStore {
    /// The receipt hashes of the members of `slice` as of `asof`, at most `limit`, in no particular order
    /// (decided 27 Nov, B6): one statement; with a `deadline`, a progress handler stops it past the deadline
    /// (the call then fails with [`Error::Timeout`]).
    fn member_hashes_by(
        &self,
        slice: &Slice,
        asof: &str,
        limit: usize,
        deadline: Option<Instant>,
        op: &'static str,
    ) -> Result<Vec<String>> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if version_slice(slice, &v)?.is_some() {
            return Ok(crate::hashes_limited(self.slice_members(slice, asof)?, limit));
        }
        let p = Pattern::of(&v)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run(op, |c| {
            // The deadline interrupts the statement from a timer thread (rusqlite's interrupt handle: the
            // crate forbids unsafe code, so no progress handler).
            let timer = deadline.map(|at| {
                let handle = c.get_interrupt_handle();
                let (stop, stopped) = std::sync::mpsc::channel::<()>();
                let timer = std::thread::spawn(move || {
                    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        stopped.recv_timeout(at.saturating_duration_since(Instant::now()))
                    {
                        handle.interrupt();
                    }
                });
                (stop, timer)
            });
            let out: Result<Vec<String>> = (|| {
                let (d, _) = drive(&p, c, false)?;
                let q = if d.is_some() { L::MemberHashesDrive } else { L::MemberHashesAll };
                let mut binds = params(&p, asof, d);
                binds.push((":limit", SqlValue::Integer(limit)));
                let mut st = c.prepare_cached(q.sql()).db()?;
                bind(&mut st, &binds)?;
                let mut rows = st.raw_query();
                let mut out = Vec::new();
                while let Some(r) = rows.next().db()? {
                    out.push(r.get::<_, String>(0).db()?);
                }
                Ok(out)
            })();
            if let Some((stop, timer)) = timer {
                let _ = stop.send(());
                let _ = timer.join();
            }
            match out {
                Err(e) if deadline.is_some_and(|at| Instant::now() >= at) => {
                    let _ = e;
                    Err(Error::Timeout(op.to_string()))
                }
                other => other,
            }
        })
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::DatabaseBusy)
}

/// The path of a `sqlite:` URL (`sqlite:<path>`, `sqlite://<path>`), or `url` itself.
pub(crate) fn sqlite_path(url: &str) -> &str {
    match url.strip_prefix("sqlite:") {
        Some(rest) => rest.strip_prefix("//").unwrap_or(rest),
        None => url,
    }
}

/// The name of a SQLite (extended) result code.
pub(crate) fn result_code_name(code: std::os::raw::c_int) -> String {
    let name = match code {
        1 => "SQLITE_ERROR",
        2 => "SQLITE_INTERNAL",
        3 => "SQLITE_PERM",
        4 => "SQLITE_ABORT",
        5 => "SQLITE_BUSY",
        6 => "SQLITE_LOCKED",
        7 => "SQLITE_NOMEM",
        8 => "SQLITE_READONLY",
        9 => "SQLITE_INTERRUPT",
        10 => "SQLITE_IOERR",
        11 => "SQLITE_CORRUPT",
        12 => "SQLITE_NOTFOUND",
        13 => "SQLITE_FULL",
        14 => "SQLITE_CANTOPEN",
        15 => "SQLITE_PROTOCOL",
        16 => "SQLITE_EMPTY",
        17 => "SQLITE_SCHEMA",
        18 => "SQLITE_TOOBIG",
        19 => "SQLITE_CONSTRAINT",
        20 => "SQLITE_MISMATCH",
        21 => "SQLITE_MISUSE",
        22 => "SQLITE_NOLFS",
        23 => "SQLITE_AUTH",
        24 => "SQLITE_FORMAT",
        25 => "SQLITE_RANGE",
        26 => "SQLITE_NOTADB",
        261 => "SQLITE_BUSY_RECOVERY",
        517 => "SQLITE_BUSY_SNAPSHOT",
        773 => "SQLITE_BUSY_TIMEOUT",
        262 => "SQLITE_LOCKED_SHAREDCACHE",
        264 => "SQLITE_READONLY_RECOVERY",
        520 => "SQLITE_READONLY_CANTLOCK",
        270 => "SQLITE_CANTOPEN_NOTEMPDIR",
        526 => "SQLITE_CANTOPEN_ISDIR",
        782 => "SQLITE_CANTOPEN_FULLPATH",
        267 => "SQLITE_CORRUPT_VTAB",
        275 => "SQLITE_CONSTRAINT_CHECK",
        531 => "SQLITE_CONSTRAINT_COMMITHOOK",
        787 => "SQLITE_CONSTRAINT_FOREIGNKEY",
        1043 => "SQLITE_CONSTRAINT_FUNCTION",
        1299 => "SQLITE_CONSTRAINT_NOTNULL",
        1555 => "SQLITE_CONSTRAINT_PRIMARYKEY",
        1811 => "SQLITE_CONSTRAINT_TRIGGER",
        2067 => "SQLITE_CONSTRAINT_UNIQUE",
        2323 => "SQLITE_CONSTRAINT_VTAB",
        2579 => "SQLITE_CONSTRAINT_ROWID",
        3091 => "SQLITE_CONSTRAINT_DATATYPE",
        _ => {
            // Another extended code: its primary code's name and the number.
            let primary = code & 0xff;
            return if primary != code && primary != 0 {
                format!("{}({code})", result_code_name(primary))
            } else {
                format!("SQLITE({code})")
            };
        }
    };
    name.to_string()
}

/// A version from a row of (tx, receipt_hash, asof, withdrawn, recorded, ...) (0.6.0: withdrawn is
/// 0 or 1; 0.6.5: recorded is the version's recorded time).
fn version_row(r: &rusqlite::Row<'_>, cell_id: &str) -> rusqlite::Result<Version> {
    Ok(Version {
        cell_id: cell_id.to_string(),
        tx: r.get(0)?,
        receipt_hash: r.get(1)?,
        asof: r.get(2)?,
        withdrawn: r.get(3)?,
        recorded: r.get(4)?,
        seq: r.get::<_, Option<i64>>("seq")?.unwrap_or(0),
    })
}

/// The latest binding row of `name` for `q` (tx, and the hash, NULL when the name is closed).
fn latest_binding_row(c: &Connection, q: L, name: &str) -> Result<Option<(String, Option<String>)>> {
    c.prepare_cached(q.sql()).db()?.query_row([name], |r| Ok((r.get(0)?, r.get(1)?))).optional().db()
}

/// The method names a class binds (K5, as `bind_class` does): `<class name>/<m>` for each
/// method the class adds or overrides, with its function hash.
fn own_method_bindings(c: &Connection, class_name: &str, class_hash: &str) -> Result<Vec<(String, String)>> {
    let Some(bytes) = get_bytes(c, L::ClassGet, class_hash)? else {
        return Err(Error::NotFound { what: "class", key: class_hash.to_string() });
    };
    let flat = ClassDescriptor::from_json(&bytes)?;
    let parent = match &flat.parent {
        Some(p) => {
            get_bytes(c, L::ClassGet, p)?.map(|b| ClassDescriptor::from_json(&b)).transpose()?.map(|d| d.methods)
        }
        None => None,
    };
    let methods: Vec<(String, String)> = flat.methods.into_iter().collect();
    Ok(methods
        .iter()
        .filter(|(m, h)| parent.as_ref().and_then(|pm| pm.get(m)) != Some(h))
        .map(|(m, h)| (format!("{class_name}/{m}"), h.clone()))
        .collect())
}

/// The host of a fully qualified function name (`host/name`): what precedes the last `/`.
fn name_host(fn_name: &str) -> &str {
    fn_name.rsplit_once('/').map_or("", |(h, _)| h)
}

fn read_namespace(c: &Connection, host: &str) -> Result<Option<Namespace>> {
    c.prepare_cached(L::NamespaceGet.sql())
        .db()?
        .query_row([host], |r| {
            Ok(Namespace {
                host: host.to_string(),
                keydoc: r.get(0)?,
                validated_at: r.get(1)?,
                suspended_at: r.get(2)?,
                consecutive_failures: r.get(3)?,
                last_checked_at: r.get(4)?,
            })
        })
        .optional()
        .db()
}

fn get_bytes(c: &Connection, q: L, key: &str) -> Result<Option<Vec<u8>>> {
    c.prepare_cached(q.sql()).db()?.query_row([key], |r| r.get(0)).optional().db()
}

fn get_strings(c: &Connection, q: L, key: &str) -> Result<Vec<String>> {
    let mut st = c.prepare_cached(q.sql()).db()?;
    let rows = st.query_map([key], |r| r.get(0)).db()?;
    rows.collect::<rusqlite::Result<Vec<String>>>().db()
}

fn exists(c: &Connection, q: L, key: &str) -> Result<bool> {
    c.prepare_cached(q.sql()).db()?.query_row([key], |r| r.get(0)).db()
}

/// Registers the functions of the text search on a connection (0.5.3): `datumesh_entry_total`
/// and `datumesh_value_total` give a row's total score in tens for a query (see `Words` and
/// `field_score`), or NULL when a word is in none of the row's fields. The search orders and
/// cuts by them in SQL.
pub(crate) fn register_text_functions(c: &Connection) -> Result<()> {
    use rusqlite::functions::FunctionFlags;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    // The sort key's part of one argument (0.6.4): the cells' trigger joins these parts.
    c.create_scalar_function("datumesh_sort_item", 2, flags, |ctx| {
        let ty: String = ctx.get(0)?;
        let value: String = ctx.get(1)?;
        Ok(crate::slices::sort_item(&ty, &value))
    })
    .db()?;
    c.create_scalar_function("datumesh_entry_total", 5, flags, |ctx| {
        let Some(words) = Words::new(&ctx.get::<String>(0)?) else { return Ok(None::<i64>) };
        let name: String = ctx.get(1)?;
        let label: String = ctx.get(2)?;
        let description: String = ctx.get(3)?;
        let args = parse_args(&ctx.get::<String>(4)?).map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
        let mut fields: Vec<&str> = vec![&name, &label, &description];
        for (k, v) in &args {
            fields.push(k);
            fields.push(v);
        }
        Ok(words.total(&fields).map(i64::from))
    })
    .db()?;
    c.create_scalar_function("datumesh_value_total", 4, flags, |ctx| {
        let Some(words) = Words::new(&ctx.get::<String>(0)?) else { return Ok(None::<i64>) };
        let value: String = ctx.get(1)?;
        let label: String = ctx.get(2)?;
        let aliases =
            parse_aliases(&ctx.get::<String>(3)?).map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
        let mut fields: Vec<&str> = vec![&value, &label];
        fields.extend(aliases.iter().map(String::as_str));
        Ok(words.total(&fields).map(i64::from))
    })
    .db()?;
    Ok(())
}

/// The counter's update of a row (0.5.3): `n` tests more (or fewer) and the deltas of their
/// results, on the row of `fn_name` ('' for unnamed). The row is made first, with the hash's
/// branch counts (or `init`).
fn bump_counter(
    c: &Connection,
    fn_hash: &str,
    fn_name: &str,
    n: i64,
    results: [i64; 4],
    last_run_at: Option<&str>,
    init: (i64, i64),
) -> Result<()> {
    exec(c, L::CounterEnsure, rusqlite::params![fn_hash, fn_name, init.0, init.1])?;
    let [pass, fail, error, not_run] = results;
    exec(c, L::CounterAdd, rusqlite::params![fn_hash, fn_name, n, pass, fail, error, not_run, last_run_at])?;
    Ok(())
}

/// A test's name and last result, if the test is stored (0.5.3).
fn function_test_old(c: &Connection, fn_hash: &str, case: &str) -> Result<Option<(Option<String>, Option<String>)>> {
    let mut st = c.prepare_cached(L::FunctionTestOld.sql()).db()?;
    let mut rows = st
        .query_map(rusqlite::params![fn_hash, case], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .db()?;
    rows.next().transpose().db()
}

fn exec(c: &Connection, q: L, params: impl rusqlite::Params) -> Result<usize> {
    c.prepare_cached(q.sql()).db()?.execute(params).db()
}

/// A stored value that does not parse back (a corrupt row).
fn bad(what: &str) -> Error {
    Error::Invalid(datumesh_core::Error::new(ErrorCode::CanonType, "$", format!("a stored {what}")))
}

/// A cell's arguments from the typed-argument index: a JSON array of `[name, type, value]`.
fn args_of(text: &str) -> Result<Args> {
    let Ok(Value::Array(list)) = json::parse(text.as_bytes()) else { return Err(bad("argument list")) };
    let mut args = Args::new();
    for a in &list {
        let item = |i: usize| match a {
            Value::Array(x) => x.get(i).and_then(Value::as_str).ok_or_else(|| bad("argument")),
            _ => Err(bad("argument")),
        };
        let ty = Type::from_name(item(1)?).ok_or_else(|| bad("type"))?;
        args.insert(item(0)?.to_string(), Arg::Value(Scalar::from_lexical(ty, item(2)?)?));
    }
    Ok(args)
}

/// The sort key of a typed value in the typed-argument index: for a number, a text that
/// sorts (as bytes) as the decimal does; for every other type, its lexical form, which
/// already sorts in the type's order.
pub(crate) fn sort_key(ty: &str, lexical: &str) -> String {
    if ty == "number" {
        decimal_key(lexical)
    } else {
        lexical.to_string()
    }
}

/// A decimal (SPEC 3.1 form: optional `-`, digits, optional `.` and digits) as a text that
/// sorts as the number does: `0` for a negative number, `1` for zero, `2` for a positive
/// one; then the decimal exponent `e` (the number is `0.d1d2... x 10^e`) plus 500, in three
/// digits; then the significant digits `d1d2...`. Longer digit strings after a shared
/// prefix are larger, as text and as numbers. For a negative number the exponent and the
/// digits are complemented (`999 - x`, `9 - d`) and `~` appended, which reverses the order,
/// prefixes included (`~` sorts after every digit). The specification's numbers have at
/// most 128 characters, so `e` lies in -127..=128.
pub(crate) fn decimal_key(lexical: &str) -> String {
    let (negative, magnitude) = match lexical.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, lexical),
    };
    let (int, frac) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let (exponent, digits): (i64, String) = if !int.is_empty() {
        (int.len() as i64, format!("{int}{frac}").trim_end_matches('0').to_string())
    } else {
        let zeros = frac.len() - frac.trim_start_matches('0').len();
        (-(zeros as i64), frac[zeros..].to_string())
    };
    if digits.is_empty() {
        return "1".to_string();
    }
    let e = (exponent + 500).clamp(0, 999);
    if negative {
        let complement: String = digits.bytes().map(|d| char::from(b'9' - (d - b'0'))).collect();
        format!("0{:03}{complement}~", 999 - e)
    } else {
        format!("2{e:03}{digits}")
    }
}

impl Resolver for SqliteStore {
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(fn_hash) {
            return None;
        }
        self.resolve("descriptor", |c| get_bytes(c, L::DescriptorGet, fn_hash)).map(Cow::Owned)
    }

    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(hash) {
            return None;
        }
        self.resolve("receipt", |c| get_bytes(c, L::ReceiptGet, hash)).map(Cow::Owned)
    }

    /// The stored manifest of slice entry `entry` ([`Store::put_manifest`]), its canonical
    /// form; `None` when there is none, or `entry` is not an entry.
    fn manifest(&self, entry: &str) -> Option<Cow<'_, [u8]>> {
        if check_entry(entry, "$").is_err() {
            return None;
        }
        self.resolve("manifest", |c| get_bytes(c, L::ManifestGet, entry)).map(Cow::Owned)
    }

    /// `Some` only for a `stored` function, as [`PgStore`](crate::PgStore)'s: the value of
    /// the receipt of [`stored_asof`](Store::stored_asof)'s version.
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<Scalar> {
        if !is_instant(asof) {
            return None;
        }
        self.resolve("evaluate", |c| {
            let Some(desc) = get_bytes(c, L::DescriptorGet, &cell.fn_hash)? else { return Ok(None) };
            match FunctionDescriptor::from_json(&desc) {
                Ok(d) if d.lang == "stored" => {}
                Ok(_) => return Ok(None),
                Err(e) => {
                    tracing::error!(target: "datumesh_store", fn_hash = %cell.fn_hash, error = %e, "stored descriptor does not parse");
                    return Ok(None);
                }
            }
            let bytes: Option<Vec<u8>> = c
                .prepare_cached(L::ValueAsof.sql())
                .db()?
                .query_row(rusqlite::params![cell.id(), asof], |r| r.get(0))
                .optional()
                .db()?;
            let Some(bytes) = bytes else { return Ok(None) };
            match stored_value(&bytes) {
                Ok(value) => Ok(value),
                Err(e) => {
                    tracing::error!(target: "datumesh_store", cell_id = %cell.id(), error = %e, "stored receipt does not parse");
                    Ok(None)
                }
            }
        })
    }
}

impl SqliteStore {
    /// A failed check (`threshold` is `Some`) or a successful one (`None`) at `checked_at`.
    fn record_check(
        &self,
        op: &'static str,
        host: &str,
        checked_at: &str,
        threshold: Option<u32>,
    ) -> Result<Option<Namespace>> {
        check_dns_name(host, "host")?;
        check_instant(checked_at, "checked_at")?;
        let out = self.write(op, |c| {
            let Some(mut ns) = read_namespace(c, host)? else { return Ok(None) };
            if let Some(at) = &ns.last_checked_at {
                if checked_at < at.as_str() {
                    return Err(Error::NotLater {
                        what: "namespace",
                        key: host.to_string(),
                        given: checked_at.to_string(),
                        latest: at.clone(),
                    });
                }
            }
            crate::apply_check(&mut ns, checked_at, threshold);
            exec(c, L::NamespaceCheck, rusqlite::params![host, ns.consecutive_failures, checked_at, ns.suspended_at])?;
            Ok(Some(ns))
        })?;
        tracing::debug!(target: "datumesh_store", host, checked_at, ok = threshold.is_none(), "namespace check");
        Ok(out)
    }
}

impl SqliteStore {
    /// Writes catalogue rows, all or nothing, keyed by (kind, name) (the call's bounds are
    /// checked by the caller). An operator's row never replaces a publisher's. An existing row
    /// keeps its facets and `extends` unless `replace_ext` (the facets call), in which case
    /// they are replaced.
    fn write_catalog(&self, rows: &[CatalogRow<'_>], replace_ext: bool) -> Result<Vec<i64>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let replace = if replace_ext {
            ", facets = excluded.facets, extends = excluded.extends, source_name = excluded.source_name"
        } else {
            ""
        };
        let upsert = format!(
            "INSERT INTO catalog_entries (kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT (kind, name) DO UPDATE SET hash = excluded.hash, label = excluded.label,
                 description = excluded.description, args = excluded.args, examples = excluded.examples,
                 source = excluded.source, signed_by = excluded.signed_by, updated_at = excluded.updated_at{replace}
             WHERE catalog_entries.source = 'operator' OR excluded.source = 'publisher'
             RETURNING id"
        );
        self.write("upsert_catalog", |c| {
            let mut ids = Vec::with_capacity(rows.len());
            for row in rows {
                let e = row.entry;
                let args = args_json(&e.args);
                let examples = canonical_examples(&e.examples)?;
                let updated: Option<i64> = c
                    .prepare_cached(&upsert)
                    .db()?
                    .query_row(
                        rusqlite::params![
                            e.kind.as_str(),
                            e.name,
                            e.hash,
                            e.label,
                            e.description,
                            args,
                            examples,
                            e.source.as_str(),
                            e.signed_by,
                            row.facets,
                            row.extends,
                            row.source_name
                        ],
                        |r| r.get(0),
                    )
                    .optional()
                    .db()?;
                let id = match updated {
                    Some(id) => id,
                    // An operator's entry over a publisher's: kept as it is.
                    None => c
                        .prepare_cached("SELECT id FROM catalog_entries WHERE kind = ?1 AND name = ?2")
                        .db()?
                        .query_row([e.kind.as_str(), e.name.as_str()], |r| r.get(0))
                        .db()?,
                };
                ids.push(id);
            }
            Ok(ids)
        })
    }
}

impl Store for SqliteStore {
    fn put_member_set_checked(&self, set: &MemberSet) -> Result<bool> {
        let members = i64::try_from(set.members).unwrap_or(i64::MAX);
        let licences = crate::licences_text(&set.licences);
        let seen_seq = set.seen_seq;
        self.write("put_member_set", |c| {
            let n = c
                .prepare_cached(L::MemberSetPut.sql())
                .db()?
                .execute(rusqlite::params![
                    set.entry,
                    set.slice_id,
                    set.members_hash,
                    set.fn_hash,
                    set.slice,
                    members,
                    set.tx,
                    set.asof,
                    licences,
                    seen_seq
                ])
                .db()?;
            Ok(n == 1)
        })
    }

    fn member_set_checked(&self, entry: &str) -> Result<Option<MemberSet>> {
        self.run("member_set", |c| {
            let row = c
                .prepare_cached(L::MemberSetGet.sql())
                .db()?
                .query_row([entry], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, i64>(8)?,
                    ))
                })
                .optional()
                .db()?;
            let Some((slice_id, members_hash, fn_hash, slice, members, tx, asof, licences, seen_seq)) = row else {
                return Ok(None);
            };
            Ok(Some(MemberSet {
                entry: entry.to_string(),
                slice_id,
                members_hash,
                fn_hash,
                slice,
                members: u64::try_from(members).unwrap_or(0),
                tx,
                asof,
                licences: crate::licences_of(&licences)?,
                seen_seq,
            }))
        })
    }

    fn put_slice_definition_checked(&self, slice: &Slice) -> Result<bool> {
        let (id, fn_hash, bytes) = (slice.id().to_string(), slice.fn_hash().to_string(), slice.canonical());
        self.write("put_slice_definition", |c| {
            let n = c
                .prepare_cached(L::SliceDefinitionPut.sql())
                .db()?
                .execute(rusqlite::params![id, fn_hash, bytes])
                .db()?;
            Ok(n == 1)
        })
    }

    fn recording_clock_of(&self) -> Option<crate::RecordingClock> {
        self.options.recording_clock.clone().map(|c| c.0)
    }

    fn slice_member_hashes(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<String>> {
        self.member_hashes_by(slice, asof, limit, None, "slice_member_hashes")
    }

    fn slice_member_hashes_within_checked(
        &self,
        slice: &Slice,
        asof: &str,
        limit: usize,
        timeout: Duration,
    ) -> Result<Vec<String>> {
        self.member_hashes_by(slice, asof, limit, Some(Instant::now() + timeout), "slice_member_hashes_within")
    }

    fn barrier_checked(&self) -> Result<()> {
        // A write transaction in flight (in this process or another) holds the write lock. The wait is the
        // barrier's own timeout, not the connections' busy timeout; the in-process queue is short (writes are).
        self.run("barrier", |c| {
            let _queue = self.writer.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            c.busy_timeout(self.options.barrier_timeout).db()?;
            let begun = c.execute_batch("BEGIN IMMEDIATE");
            c.busy_timeout(self.options.busy_timeout).db()?;
            if let Err(e) = &begun {
                if is_busy(e) {
                    return Err(Error::Timeout("barrier".to_string()));
                }
            }
            begun.db()?;
            c.execute_batch("COMMIT").db()?;
            Ok(())
        })
    }

    fn later_bound_checked(&self, cell_id: &str, recorded: &str) -> Result<Option<String>> {
        self.run("later_bound", |c| {
            c.prepare_cached(L::LaterBound.sql())
                .db()?
                .query_row(rusqlite::params![cell_id, recorded], |r| r.get::<_, Option<String>>(0))
                .db()
        })
    }

    fn next_version_after_checked(&self, cell_id: &str, recorded: &str) -> Result<Option<Version>> {
        self.run("next_version_after", |c| {
            c.prepare_cached(L::VersionNextAfter.sql())
                .db()?
                .query_row(rusqlite::params![cell_id, recorded], |r| version_row(r, cell_id))
                .optional()
                .db()
        })
    }

    fn fn_seq_checked(&self, fn_hash: &str) -> Result<i64> {
        self.run("fn_seq", |c| c.prepare_cached(L::FnSeq.sql()).db()?.query_row([fn_hash], |r| r.get(0)).db())
    }

    fn slice_seq_checked(&self, slice: &Slice) -> Result<i64> {
        let v = slice.to_value();
        if let Some((cell_id, _, _)) = version_slice(slice, &v)? {
            check_hex(&cell_id, 64, "cell_id")?;
            return self.run("slice_seq", |c| {
                c.prepare_cached(L::VersionSeqMax.sql()).db()?.query_row([cell_id.as_str()], |r| r.get(0)).db()
            });
        }
        let p = Pattern::of(&v)?;
        self.run("slice_seq", |c| {
            let seqs = rows(&p, c, "", [L::SliceSeqAll, L::SliceSeqDrive], |r| r.get::<_, i64>(0))?;
            Ok(seqs.into_iter().max().unwrap_or(0))
        })
    }

    fn slice_definition_checked(&self, slice_id: &str) -> Result<Option<Slice>> {
        let bytes: Option<Vec<u8>> = self.run("slice_definition", |c| {
            c.prepare_cached(L::SliceDefinitionGet.sql()).db()?.query_row([slice_id], |r| r.get(0)).optional().db()
        })?;
        bytes.map(|b| Slice::from_json(&b).map_err(Error::Invalid)).transpose()
    }

    fn put_descriptor(&self, bytes: &[u8]) -> Result<String> {
        let d = FunctionDescriptor::from_json(bytes)?;
        let hash = d.hash();
        let canonical = d.canonical();
        // The descriptor (once) and its inputs' index (0.5.1), in one write transaction.
        self.run("put_descriptor", |c| {
            let fresh = !exists(c, L::DescriptorExists, &hash)?;
            if fresh || !d.inputs.is_empty() {
                self.locked_write(c, |c| {
                    exec(c, L::DescriptorPut, rusqlite::params![hash, canonical])?;
                    for input in &d.inputs {
                        exec(c, L::DescriptorInputPut, rusqlite::params![hash, input])?;
                    }
                    Ok(())
                })?;
                self.bound_wal(c);
            }
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_hash = %hash, "put_descriptor");
        Ok(hash)
    }

    fn put_receipt(&self, bytes: &[u8]) -> Result<String> {
        // Steps 1-4 first (a valued receipt or a withdrawal): one that is neither is refused
        // without a query.
        let p = parse_receipt(bytes)?;
        let canonical = p.canonical.clone();
        let hash = p.hash.clone();
        let cell = &p.cell;
        let cell_id = p.cell_id.clone();
        let host = p.host.clone();
        let (mut entries, mut inputs) = (p.entries.clone(), p.inputs.clone());
        entries.sort();
        entries.dedup();
        inputs.sort();
        inputs.dedup();
        let mut args = Vec::with_capacity(cell.args.len());
        for (name, a) in &cell.args {
            let Some(s) = a.scalar() else {
                return Err(Error::Invalid(datumesh_core::Error::new(
                    ErrorCode::Schema,
                    "$.cell.args",
                    "a receipt's cell has no open argument",
                )));
            };
            let ty = s.ty().as_str();
            args.push((name.clone(), ty, s.lexical().to_string(), sort_key(ty, s.lexical())));
        }
        self.run("put_receipt", |c| {
            // Idempotent: the very receipt already stored verified when it was stored.
            if let Some(stored) = get_bytes(c, L::ReceiptGet, &hash)? {
                if stored == canonical {
                    return Ok(());
                }
            }
            // Steps 5-6 against the stored key document of the signer host (outside the
            // write lock: verifying is the slow part).
            let mut keys = Keys::new();
            if let Some(ns) = read_namespace(c, &host)? {
                // No new writes under a suspended namespace.
                if let Some(since) = ns.suspended_at {
                    return Err(Error::NamespaceSuspended { host: host.clone(), suspended_at: since });
                }
                keys.insert_response(&host, 200, Some(&ns.keydoc));
            }
            let out = verify(bytes, &keys, None, None);
            if !out.ok {
                return Err(Error::Invalid(out.error().unwrap_or_else(|| {
                    datumesh_core::Error::new(ErrorCode::Signature, "$", "the receipt did not verify")
                })));
            }
            self.locked_write(c, |c| {
                // Under the write lock: another writer may have stored it meanwhile.
                if let Some(stored) = get_bytes(c, L::ReceiptGet, &hash)? {
                    return if stored == canonical { Ok(()) } else { Err(Error::Conflict { hash: hash.clone() }) };
                }
                // A withdrawal has no value: its columns are the empty text (0.6.0).
                let (value_type, value) = match &p.value {
                    Some((ty, lexical)) => (ty.as_str(), lexical.as_str()),
                    None => (WITHDRAWN_VALUE, WITHDRAWN_VALUE),
                };
                exec(
                    c,
                    L::ReceiptPut,
                    rusqlite::params![
                        hash,
                        canonical,
                        cell_id,
                        p.tx,
                        cell.fn_hash,
                        p.publisher,
                        p.asof,
                        value_type,
                        value
                    ],
                )?;
                for i in &inputs {
                    exec(c, L::EdgePut, rusqlite::params![hash, i])?;
                }
                for e in &entries {
                    exec(c, L::SliceEdgePut, rusqlite::params![hash, e])?;
                }
                if !exists(c, L::CellExists, &cell_id)? {
                    exec(
                        c,
                        L::CellPut,
                        rusqlite::params![cell_id, cell.fn_hash, cell.ns, cell.fn_name, args.len() as i64],
                    )?;
                    for (name, ty, value, key) in &args {
                        exec(c, L::CellArgPut, rusqlite::params![cell_id, name, ty, value, key, cell.fn_hash])?;
                    }
                }
                Ok(())
            })?;
            self.bound_wal(c);
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", hash = %hash, cell_id = %cell_id, "put_receipt");
        Ok(hash)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let length = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        self.run("put_blob", |c| {
            if exists(c, L::BlobExists, &sha)? {
                return Ok(());
            }
            self.locked_write(c, |c| exec(c, L::BlobPut, rusqlite::params![sha, bytes, length]).map(|_| ()))?;
            self.bound_wal(c);
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", sha256 = %sha, length, "put_blob");
        Ok(sha)
    }

    fn blob(&self, sha256: &str) -> Result<Option<Cow<'_, [u8]>>> {
        check_hex(sha256, 64, "sha256")?;
        Ok(self.run("blob", |c| get_bytes(c, L::BlobGet, sha256))?.map(Cow::Owned))
    }

    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(tx, "tx")?;
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.write("put_version", |c| {
            // A store on a schema older than migration 17 writes by tx, as 0.6.4 did (decision B).
            let has = if self.recorded_schema.load(std::sync::atomic::Ordering::Acquire) {
                true
            } else {
                let has = c.prepare_cached(L::HasRecorded.sql()).db()?.query_row([], |r| r.get::<_, i64>(0)).db()? == 1;
                if has {
                    self.recorded_schema.store(true, std::sync::atomic::Ordering::Release);
                }
                has
            };
            // Recorded after the cell's latest (decided 27 Nov, B5); this connection holds the write lock.
            let latest_now =
                get_strings(c, if has { L::VersionLatestTx } else { L::VersionLatestTxLegacy }, cell_id)?.pop();
            let recorded = if has {
                let base = crate::recorded_at(self.options.recording_clock.as_ref().map(|c| &c.0), tx);
                match &latest_now {
                    Some(l) => crate::later_of(&base, &crate::instant_after(l)),
                    None => base,
                }
            } else {
                tx.to_string()
            };
            let meta: Option<(String, String)> = c
                .prepare_cached(L::ReceiptMeta.sql())
                .db()?
                .query_row([receipt_hash], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .db()?;
            let Some((r_cell, r_tx)) = meta else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            // No new versions of a receipt signed by a suspended namespace.
            let stored = get_bytes(c, L::ReceiptGet, receipt_hash)?
                .ok_or_else(|| Error::NotFound { what: "receipt", key: receipt_hash.to_string() })?;
            let signer = parse_receipt(&stored)?.host;
            if let Some(since) = read_namespace(c, &signer)?.and_then(|n| n.suspended_at) {
                return Err(Error::NamespaceSuspended { host: signer, suspended_at: since });
            }
            if r_cell != cell_id {
                return Err(Error::Mismatch { what: "cell_id", expected: cell_id.to_string(), found: r_cell });
            }
            if r_tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: r_tx });
            }
            // The cell already has a version at this tx: the same receipt writes nothing; another is refused
            // (review item 3).
            if let Some(held) = c
                .prepare_cached(L::VersionAt.sql())
                .db()?
                .query_row(rusqlite::params![cell_id, tx], |r| r.get::<_, String>(0))
                .optional()
                .db()?
            {
                if held == receipt_hash {
                    return Ok(());
                }
                return Err(Error::NotLater {
                    what: "version",
                    key: cell_id.to_string(),
                    given: tx.to_string(),
                    latest: tx.to_string(),
                });
            }
            // The cell's latest signed tx: a version not later is refused (decided by the user, 27 Nov).
            if let Some(l) = get_strings(c, L::VersionLatestSigned, cell_id)?.pop() {
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "version",
                        key: cell_id.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            if let Some(l) = &latest_now {
                if recorded.as_str() <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "version",
                        key: cell_id.to_string(),
                        given: recorded.clone(),
                        latest: l.clone(),
                    });
                }
            }
            if has {
                exec(c, L::VersionPut, rusqlite::params![cell_id, tx, receipt_hash, recorded])?;
            } else {
                exec(c, L::VersionPutLegacy, rusqlite::params![cell_id, tx, receipt_hash])?;
            }
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", cell_id, tx, receipt_hash, "put_version");
        Ok(())
    }

    fn version_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        self.asof_version("version_asof", L::VersionAsof, cell_id, asof)
    }

    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Result<Vec<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(from, "from")?;
        check_instant(to, "to")?;
        self.run("versions", |c| {
            let mut st = c.prepare_cached(L::VersionRange.sql()).db()?;
            let rows = st.query_map(rusqlite::params![cell_id, from, to], |r| version_row(r, cell_id)).db()?;
            rows.collect::<rusqlite::Result<Vec<Version>>>().db()
        })
    }

    fn latest(&self, cell_id: &str) -> Result<Option<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        self.run("latest", |c| {
            c.prepare_cached(L::VersionLatest.sql())
                .db()?
                .query_row([cell_id], |r| version_row(r, cell_id))
                .optional()
                .db()
        })
    }

    fn dependents(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("dependents", |c| get_strings(c, L::Dependents, receipt_hash))
    }

    fn inputs(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("inputs", |c| get_strings(c, L::Inputs, receipt_hash))
    }

    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()> {
        keys_from_response(host, 200, Some(keydoc_bytes))?;
        check_instant(validated_at, "validated_at")?;
        let canonical = json::parse(keydoc_bytes)?.to_jcs();
        self.write("put_namespace", |c| {
            let at: Option<String> =
                c.prepare_cached(L::NamespaceGet.sql()).db()?.query_row([host], |r| r.get(1)).optional().db()?;
            if let Some(at) = at {
                if validated_at < at.as_str() {
                    return Err(Error::NotLater {
                        what: "namespace",
                        key: host.to_string(),
                        given: validated_at.to_string(),
                        latest: at,
                    });
                }
            }
            exec(c, L::NamespacePut, rusqlite::params![host, canonical, validated_at])?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", host, validated_at, "put_namespace");
        Ok(())
    }

    fn namespace(&self, host: &str) -> Result<Option<Namespace>> {
        check_dns_name(host, "host")?;
        self.run("namespace", |c| read_namespace(c, host))
    }

    fn namespaces(&self) -> Result<Vec<Namespace>> {
        self.run("namespaces", |c| {
            let mut st = c.prepare_cached(L::NamespaceList.sql()).db()?;
            let rows = st
                .query_map([], |r| {
                    Ok(Namespace {
                        host: r.get(0)?,
                        keydoc: r.get(1)?,
                        validated_at: r.get(2)?,
                        suspended_at: r.get(3)?,
                        consecutive_failures: r.get(4)?,
                        last_checked_at: r.get(5)?,
                    })
                })
                .db()?;
            rows.collect::<rusqlite::Result<Vec<Namespace>>>().db()
        })
    }

    fn record_namespace_failure(&self, host: &str, checked_at: &str, threshold: u32) -> Result<Option<Namespace>> {
        self.record_check("record_namespace_failure", host, checked_at, Some(threshold))
    }

    fn record_namespace_success(&self, host: &str, checked_at: &str) -> Result<Option<Namespace>> {
        self.record_check("record_namespace_success", host, checked_at, None)
    }

    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()> {
        check_function_name(fn_name, "fn_name")?;
        check_hex(fn_hash, 64, "fn_hash")?;
        check_instant(tx, "tx")?;
        self.write("bind_name", |c| {
            // No new bindings under a suspended namespace.
            let host = name_host(fn_name);
            if let Some(since) = read_namespace(c, host)?.and_then(|n| n.suspended_at) {
                return Err(Error::NamespaceSuspended { host: host.to_string(), suspended_at: since });
            }
            // One namespace of names: a class name is not a function name.
            if exists(c, L::ClassNameBound, fn_name)? {
                return Err(Error::NameTaken { name: fn_name.to_string(), bound_as: "class" });
            }
            if !exists(c, L::DescriptorExists, fn_hash)? {
                return Err(Error::NotFound { what: "descriptor", key: fn_hash.to_string() });
            }
            if let Some(l) = get_strings(c, L::NameLatest, fn_name)?.pop() {
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "name",
                        key: fn_name.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            exec(c, L::NamePut, rusqlite::params![fn_name, tx, fn_hash])?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_name, fn_hash, tx, "bind_name");
        Ok(())
    }

    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_name", |c| {
            // NULL for a closed binding (0.6.0): the name means nothing from then on.
            let hash: Option<Option<String>> =
                c.prepare_cached(L::NameAsof.sql()).db()?.query_row([fn_name, asof], |r| r.get(0)).optional().db()?;
            Ok(hash.flatten())
        })
    }

    fn put_class(&self, bytes: &[u8]) -> Result<String> {
        // The core checks the descriptor (strict JSON, then 16.1); the bytes must be its
        // canonical form, since the class hash is the hash of the bytes stored.
        let d = ClassDescriptor::from_json(bytes)?;
        let canonical = d.canonical();
        if canonical != bytes {
            return Err(not_canonical_class());
        }
        let hash = d.hash();
        let host = d.steward.namespace.clone();
        self.write("put_class", |c| {
            // No new writes under a suspended namespace: the steward's (16.2).
            if let Some(since) = read_namespace(c, &host)?.and_then(|n| n.suspended_at) {
                return Err(Error::NamespaceSuspended { host: host.clone(), suspended_at: since });
            }
            exec(c, L::ClassPut, rusqlite::params![hash, canonical])?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", class_hash = %hash, "put_class");
        Ok(hash)
    }

    fn class(&self, class_hash: &str) -> Result<Option<Vec<u8>>> {
        check_hex(class_hash, 64, "class_hash")?;
        self.run("class", |c| get_bytes(c, L::ClassGet, class_hash))
    }

    fn bind_class(&self, class_name: &str, class_hash: &str, tx: &str) -> Result<()> {
        check_function_name(class_name, "class_name")?;
        check_hex(class_hash, 64, "class_hash")?;
        check_instant(tx, "tx")?;
        self.write("bind_class", |c| {
            // No new bindings under a suspended namespace: the host of the class name.
            let host = name_host(class_name);
            if let Some(since) = read_namespace(c, host)?.and_then(|n| n.suspended_at) {
                return Err(Error::NamespaceSuspended { host: host.to_string(), suspended_at: since });
            }
            let Some(bytes) = get_bytes(c, L::ClassGet, class_hash)? else {
                return Err(Error::NotFound { what: "class", key: class_hash.to_string() });
            };
            let flat = ClassDescriptor::from_json(&bytes)?;
            let methods: Vec<(String, String)> = flat.methods.into_iter().collect();
            // K5: only the methods this class adds or overrides are bound under its name: those
            // whose function hash the parent's flattened descriptor does not give.
            let parent = match &flat.parent {
                Some(p) => get_bytes(c, L::ClassGet, p)?
                    .map(|b| ClassDescriptor::from_json(&b))
                    .transpose()?
                    .map(|d| d.methods),
                None => None,
            };
            let own: Vec<(String, String)> =
                methods.iter().filter(|(m, h)| parent.as_ref().and_then(|pm| pm.get(m)) != Some(h)).cloned().collect();
            let own_names: Vec<String> = own.iter().map(|(m, _)| format!("{class_name}/{m}")).collect();
            // Every method's function descriptor is stored.
            for (_, h) in &methods {
                if !exists(c, L::DescriptorExists, h)? {
                    return Err(Error::NotFound { what: "descriptor", key: h.clone() });
                }
            }
            // One namespace of names: the class name is not a function name, and no method
            // name is a class name.
            if exists(c, L::NameBound, class_name)? {
                return Err(Error::NameTaken { name: class_name.to_string(), bound_as: "function" });
            }
            for name in &own_names {
                if exists(c, L::ClassNameBound, name)? {
                    return Err(Error::NameTaken { name: name.clone(), bound_as: "class" });
                }
            }
            // Later than the latest binding of the class name and of each method name.
            if let Some(l) = get_strings(c, L::ClassBindingLatest, class_name)?.pop() {
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "name",
                        key: class_name.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            for name in &own_names {
                if let Some(l) = get_strings(c, L::NameLatest, name)?.pop() {
                    if tx <= l.as_str() {
                        return Err(Error::NotLater {
                            what: "name",
                            key: name.clone(),
                            given: tx.to_string(),
                            latest: l,
                        });
                    }
                }
            }
            exec(c, L::ClassBindingPut, rusqlite::params![class_name, tx, class_hash])?;
            for ((_, h), name) in own.iter().zip(&own_names) {
                exec(c, L::NamePut, rusqlite::params![name.as_str(), tx, h.as_str()])?;
            }
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", class_name, class_hash, tx, "bind_class");
        Ok(())
    }

    fn close_binding(&self, name: &str, at: &str) -> Result<()> {
        check_function_name(name, "name")?;
        check_instant(at, "at")?;
        self.write("close_binding", |c| {
            // No new writes under a suspended namespace: the host of the name.
            let host = name_host(name);
            if let Some(since) = read_namespace(c, host)?.and_then(|n| n.suspended_at) {
                return Err(Error::NamespaceSuspended { host: host.to_string(), suspended_at: since });
            }
            if exists(c, L::ClassNameBound, name)? {
                // The class name, then the method names its binding bound and that are still bound.
                let (tx, class_hash) =
                    latest_binding_row(c, L::ClassBindingLatestRow, name)?.ok_or_else(|| unbound(name))?;
                let class_hash = class_hash.ok_or_else(|| unbound(name))?;
                later_than("name", name, at, &tx)?;
                let mut closing: Vec<String> = Vec::new();
                for (m, _) in own_method_bindings(c, name, &class_hash)? {
                    if let Some((mtx, mhash)) = latest_binding_row(c, L::NameLatestRow, &m)? {
                        if mhash.is_some() {
                            later_than("name", &m, at, &mtx)?;
                            closing.push(m);
                        }
                    }
                }
                exec(c, L::ClassBindingClose, rusqlite::params![name, at])?;
                for m in &closing {
                    exec(c, L::NameClose, rusqlite::params![m, at])?;
                }
            } else {
                let (tx, fn_hash) = latest_binding_row(c, L::NameLatestRow, name)?.ok_or_else(|| unbound(name))?;
                fn_hash.ok_or_else(|| unbound(name))?;
                later_than("name", name, at, &tx)?;
                exec(c, L::NameClose, rusqlite::params![name, at])?;
            }
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", name, at, "close_binding");
        Ok(())
    }

    fn resolve_class(&self, class_name: &str, asof: &str) -> Result<Option<String>> {
        Ok(self.resolve_class_binding(class_name, asof)?.map(|b| b.class_hash))
    }

    fn resolve_class_binding(&self, class_name: &str, asof: &str) -> Result<Option<ClassBinding>> {
        check_function_name(class_name, "class_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_class_binding", |c| {
            // A closed binding (0.6.0) has no class hash: the name is unbound from its tx on.
            let row: Option<(Option<String>, String)> = c
                .prepare_cached(L::ClassBindingAsof.sql())
                .db()?
                .query_row([class_name, asof], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .db()?;
            Ok(row.and_then(|(h, tx)| h.map(|class_hash| ClassBinding { class_hash, tx })))
        })
    }

    fn class_names_bound_to(&self, class_hash: &str, at: &str) -> Result<Vec<String>> {
        check_hex(class_hash, 64, "class_hash")?;
        check_instant(at, "at")?;
        self.run("class_names_bound_to", |c| {
            let mut st = c.prepare_cached(L::ClassNamesBound.sql()).db()?;
            let rows = st.query_map([class_hash, at], |r| r.get::<_, String>(0)).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>().db()
        })
    }

    fn resolve_binding(&self, fn_name: &str, asof: &str) -> Result<Option<Binding>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_binding", |c| {
            // A closed binding (0.6.0) has no function hash: the name is unbound from its tx on.
            let row: Option<(Option<String>, String)> = c
                .prepare_cached(L::BindingAsof.sql())
                .db()?
                .query_row([fn_name, asof], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .db()?;
            Ok(row.and_then(|(h, tx)| h.map(|fn_hash| Binding { fn_hash, tx })))
        })
    }

    fn put_computed(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(tx, "tx")?;
        check_hex(receipt_hash, 64, "receipt_hash")?;
        let asof = self.write("put_computed", |c| {
            let meta: Option<(String, String, String, bool)> = c
                .prepare_cached(L::ReceiptMeta.sql())
                .db()?
                .query_row([receipt_hash], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .optional()
                .db()?;
            let Some((r_cell, r_tx, asof, withdrawn)) = meta else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            // A computed version states a value: a withdrawal is refused (0.6.0).
            if withdrawn {
                return Err(Error::Invalid(datumesh_core::Error::new(
                    ErrorCode::Schema,
                    "$",
                    "a withdrawal is not the receipt of a computed version",
                )));
            }
            if r_cell != cell_id {
                return Err(Error::Mismatch { what: "cell_id", expected: cell_id.to_string(), found: r_cell });
            }
            if r_tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: r_tx });
            }
            let stored: Option<String> = c
                .prepare_cached(L::ComputedGet.sql())
                .db()?
                .query_row([cell_id, tx, asof.as_str()], |r| r.get(0))
                .optional()
                .db()?;
            match stored {
                Some(h) if h == receipt_hash => {}
                Some(_) => return Err(Error::Conflict { hash: receipt_hash.to_string() }),
                None => {
                    exec(c, L::ComputedPut, rusqlite::params![cell_id, tx, asof, receipt_hash])?;
                }
            }
            Ok(asof)
        })?;
        tracing::debug!(target: "datumesh_store", cell_id, tx, asof, receipt_hash, "put_computed");
        Ok(())
    }

    fn computed_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        self.asof_version("computed_asof", L::ComputedAsof, cell_id, asof)
    }

    fn stored_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        self.asof_version("stored_asof", L::StoredAsof, cell_id, asof)
    }

    fn cell_dependents(&self, cell_id: &str) -> Result<Vec<String>> {
        check_hex(cell_id, 64, "cell_id")?;
        self.run("cell_dependents", |c| get_strings(c, L::CellDependents, cell_id))
    }

    /// One query: the version and its receipt's bytes.
    fn computed_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        Ok(self
            .asof_with_receipt("computed_asof_with_receipt", L::ComputedAsofReceipt, cell_id, asof)?
            .map(|(v, b)| (v, Cow::Owned(b))))
    }

    /// One query: the version and its receipt's bytes.
    fn stored_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        Ok(self
            .asof_with_receipt("stored_asof_with_receipt", L::StoredAsofReceipt, cell_id, asof)?
            .map(|(v, b)| (v, Cow::Owned(b))))
    }

    fn put_manifest(&self, bytes: &[u8]) -> Result<String> {
        let m = Manifest::from_json(bytes)?;
        let entry = m.entry();
        let canonical = m.canonical();
        let members = m.members();
        let n = i64::try_from(members.len()).unwrap_or(i64::MAX);
        self.run("put_manifest", |c| {
            // The entry hashes the content: a stored entry has these very bytes.
            if exists(c, L::ManifestExists, &entry)? {
                return Ok(());
            }
            self.locked_write(c, |c| {
                if exists(c, L::ManifestExists, &entry)? {
                    return Ok(());
                }
                exec(
                    c,
                    L::ManifestPut,
                    rusqlite::params![entry, m.slice().id(), m.members_hash(), m.slice().fn_hash(), n, canonical],
                )?;
                let mut st = c.prepare_cached(L::ManifestMemberPut.sql()).db()?;
                for h in members {
                    st.execute([entry.as_str(), h.as_str()]).db()?;
                }
                Ok(())
            })?;
            self.bound_wal(c);
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", entry = %entry, members = n, "put_manifest");
        Ok(entry)
    }

    fn member_manifests(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("member_manifests", |c| get_strings(c, L::MemberManifests, receipt_hash))
    }

    fn slice_entries(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("slice_entries", |c| get_strings(c, L::SliceEntries, receipt_hash))
    }

    fn slice_dependents(&self, entry: &str) -> Result<Vec<String>> {
        check_entry(entry, "entry")?;
        self.run("slice_dependents", |c| get_strings(c, L::SliceDependents, entry))
    }

    fn slice_members(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceMember>> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.versions_between(&cell_id, &from, &to, asof);
        }
        let p = Pattern::of(&v)?;
        self.run("slice_members", |c| {
            let rows = rows(&p, c, asof, [L::SliceMembersAll, L::SliceMembersDrive], |r| {
                let cell_id: String = r.get(5)?;
                Ok((
                    SliceMember {
                        // A slice's members leave the withdrawn versions out (the query does).
                        version: Version {
                            cell_id,
                            tx: r.get(0)?,
                            receipt_hash: r.get(1)?,
                            asof: r.get(2)?,
                            withdrawn: false,
                            recorded: r.get(3)?,
                            seq: r.get::<_, Option<i64>>("seq")?.unwrap_or(0),
                        },
                        receipt: r.get(4)?,
                    },
                    r.get::<_, String>(6)?,
                ))
            })?;
            in_key_order(&p, rows, |m| &m.version.cell_id)
        })
    }

    fn versions_between(&self, cell_id: &str, from: &str, to: &str, asof: &str) -> Result<Vec<SliceMember>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(from, "from")?;
        check_instant(to, "to")?;
        check_instant(asof, "asof")?;
        self.run("versions_between", |c| {
            let mut st = c.prepare_cached(L::VersionsBetween.sql()).db()?;
            let rows = st
                .query_map(rusqlite::params![cell_id, from, to, asof], |r| {
                    Ok(SliceMember { version: version_row(r, cell_id)?, receipt: r.get(5)? })
                })
                .db()?;
            rows.collect::<rusqlite::Result<Vec<SliceMember>>>().db()
        })
    }

    fn slice_values(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceValue>> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.run("slice_values", |c| {
                let mut st = c.prepare_cached(L::VersionsBetweenValues.sql()).db()?;
                let rows = st.query_map(rusqlite::params![cell_id, from, to, asof], value_columns).db()?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(value_of(r.db()?)?.0);
                }
                Ok(out)
            });
        }
        let p = Pattern::of(&v)?;
        self.run("slice_values", |c| {
            let rows = rows(&p, c, asof, [L::SliceValuesAll, L::SliceValuesDrive], value_columns)?;
            let mut values = Vec::with_capacity(rows.len());
            for row in rows {
                values.push(value_of(row)?);
            }
            in_key_order_parsed(&p, values, |m| &m.version.cell_id)
        })
    }

    fn slice_withdrawn_latest(&self, slice: &Slice, asof: &str) -> Result<Option<(String, String)>> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            // A version slice: its cell's version answering as of asof, when a withdrawal in range.
            return Ok(self
                .stored_asof(&cell_id, asof)?
                .filter(|s| s.withdrawn && from.as_str() <= s.tx.as_str() && s.tx.as_str() <= to.as_str())
                .map(|s| (s.tx, s.asof)));
        }
        let p = Pattern::of(&v)?;
        self.run("slice_withdrawn_latest", |c| withdrawn_latest(&p, c, asof))
    }

    fn slice_changed_after(&self, slice: &Slice, asof: &str) -> Result<bool> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.run("slice_changed_after", |c| {
                c.prepare_cached(L::VersionsChangedAfter.sql())
                    .db()?
                    .query_row(rusqlite::params![cell_id, from, to, asof], |r| r.get(0))
                    .db()
            });
        }
        let p = Pattern::of(&v)?;
        self.run("slice_changed_after", |c| changed_after(&p, c, asof))
    }

    fn computed_cells(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<Cell>> {
        check_instant(asof, "asof")?;
        let p = Pattern::of(&slice.to_value())?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("computed_cells", |c| {
            let (d, _) = drive(&p, c, false)?;
            let q = if d.is_some() { L::ComputedCellsDrive } else { L::ComputedCellsAll };
            let mut binds = params(&p, asof, d);
            binds.push((":limit", SqlValue::Integer(limit)));
            let mut st = c.prepare_cached(q.sql()).db()?;
            bind(&mut st, &binds)?;
            let mut rows = st.raw_query();
            let mut out = Vec::new();
            while let Some(r) = rows.next().db()? {
                let args = args_of(&r.get::<_, String>(3).db()?)?;
                out.push(Cell {
                    args,
                    fn_hash: r.get::<_, String>(2).db()?,
                    fn_name: r.get::<_, String>(1).db()?,
                    ns: r.get::<_, String>(0).db()?,
                });
            }
            Ok(out)
        })
    }

    fn has_bindings_under(&self, namespace: &str) -> Result<bool> {
        let prefix = format!("{namespace}/");
        self.run("has_bindings_under", |c| {
            c.prepare_cached(L::HasBindingsUnder.sql()).db()?.query_row([&prefix], |r| r.get(0)).db()
        })
    }

    fn upsert_catalog(&self, entries: &[CatalogEntry]) -> Result<Vec<i64>> {
        for e in entries {
            check_catalog_entry(e)?;
        }
        let rows: Vec<CatalogRow> = entries.iter().map(CatalogRow::plain).collect();
        self.write_catalog(&rows, false)
    }

    fn upsert_catalog_ext(&self, entries: &[CatalogEntryExt]) -> Result<Vec<i64>> {
        for e in entries {
            check_catalog_ext(e)?;
        }
        let rows: Vec<CatalogRow> = entries.iter().map(CatalogRow::ext).collect();
        self.write_catalog(&rows, true)
    }

    fn upsert_key_values(&self, owner: &str, key: &str, values: &[KeyValue]) -> Result<Vec<i64>> {
        check_key_values(owner, key, values)?;
        if values.is_empty() {
            return Ok(Vec::new());
        }
        self.write("upsert_key_values", |c| {
            let mut ids = Vec::with_capacity(values.len());
            for v in values {
                let aliases = aliases_json(&v.aliases);
                let id: i64 = c
                    .prepare_cached(
                        "INSERT INTO key_values (owner, key, value, label, aliases) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT (owner, key, value) DO UPDATE SET label = excluded.label, aliases = excluded.aliases
                         RETURNING id",
                    )
                    .db()?
                    .query_row(rusqlite::params![owner, key, v.value, v.label, aliases], |r| r.get(0))
                    .db()?;
                ids.push(id);
            }
            Ok(ids)
        })
    }

    fn refresh_value_counts(&self) -> Result<u64> {
        self.write("refresh_value_counts", |c| {
            let n = c
                .prepare_cached(
                    "UPDATE key_values SET n_cells = COALESCE((
                         SELECT count(*) FROM catalog_entries e JOIN cell_args a
                           ON a.fn_hash = e.hash AND a.name = key_values.key AND a.value = key_values.value
                          WHERE e.kind = 'function' AND e.name = key_values.owner), 0)",
                )
                .db()?
                .execute([])
                .db()?;
            Ok(n as u64)
        })
    }

    fn text_find(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(words) = Words::new(query) else { return Ok(Vec::new()) };
        if limit == 0 {
            return Ok(Vec::new());
        }
        // The query is $1 (the scores are computed by the functions registered on each
        // connection, which split the query the same way); the prefilter's patterns follow;
        // the LIMIT is last. Each table is ordered and cut in SQL (0.5.3).
        let entry_where = like_where_sqlite(&words, &["name", "label", "description", "args"], 2);
        let value_where = like_where_sqlite(&words, &["value", "label", "aliases"], 2);
        let cap = limit;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("text_find", |c| {
            let mut hits = Vec::new();
            let mut params: Vec<SqlValue> = vec![SqlValue::Text(query.to_string())];
            params.extend(entry_where.1.iter().map(|p| SqlValue::Text(p.clone())));
            params.push(SqlValue::Integer(limit));
            let sql = format!(
                "SELECT * FROM (SELECT id, kind, name, hash, label, description, args, examples, source, signed_by,
                        datumesh_entry_total(?1, name, label, description, args) AS total
                   FROM catalog_entries WHERE 1 = 1{}) e
                  WHERE total IS NOT NULL ORDER BY total DESC, name LIMIT ?{}",
                entry_where.0,
                params.len()
            );
            let rows: Vec<HitRow> = {
                let mut st = c.prepare(&sql).db()?;
                let mapped = st
                    .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                            r.get(9)?,
                            r.get(10)?,
                        ))
                    })
                    .db()?;
                mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
            };
            for (id, kind, name, hash, label, description, args, examples, source, signed_by, total) in rows {
                let entry =
                    entry_from_columns(&kind, name, hash, label, description, &args, &examples, &source, signed_by)?;
                hits.push(Hit::Entry { id, entry, score: words.score_of(total as u32) });
            }
            let mut params: Vec<SqlValue> = vec![SqlValue::Text(query.to_string())];
            params.extend(value_where.1.iter().map(|p| SqlValue::Text(p.clone())));
            params.push(SqlValue::Integer(limit));
            let sql = format!(
                "SELECT * FROM (SELECT id, owner, key, value, label, aliases,
                        datumesh_value_total(?1, value, label, aliases) AS total
                   FROM key_values WHERE 1 = 1{}) v
                  WHERE total IS NOT NULL
                  ORDER BY total DESC, owner || char(1) || key || char(1) || value LIMIT ?{}",
                value_where.0,
                params.len()
            );
            let rows: Vec<(i64, String, String, String, String, String, i64)> = {
                let mut st = c.prepare(&sql).db()?;
                let mapped = st
                    .query_map(rusqlite::params_from_iter(params.iter()), |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                    })
                    .db()?;
                mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
            };
            for (id, owner, key, value, label, aliases_text, total) in rows {
                let aliases = parse_aliases(&aliases_text)?;
                hits.push(Hit::Value { id, owner, key, value, label, aliases, score: words.score_of(total as u32) });
            }
            Ok(rank(hits, cap))
        })
    }

    fn vector_find(&self, _model: &str, _vec: &[f32], _limit: usize) -> Result<Vec<Hit>> {
        Err(unsupported_vectors())
    }

    fn put_embeddings(&self, _model: &str, _items: &[Embedding]) -> Result<()> {
        Err(unsupported_vectors())
    }

    fn pending_embeddings(&self, _model: &str, _limit: usize) -> Result<Vec<PendingEmbedding>> {
        Ok(Vec::new())
    }

    fn vector_available(&self) -> bool {
        false
    }

    fn log_miss(&self, query: &str) -> Result<()> {
        let Some(q) = miss_text(query) else { return Ok(()) };
        self.write("log_miss", |c| {
            c.prepare_cached("INSERT INTO find_misses (query) VALUES (?1)").db()?.execute([q]).db()?;
            // Keep the newest FIND_MISSES_CAP rows: prune the oldest beyond that.
            c.prepare_cached(
                "DELETE FROM find_misses WHERE rowid IN (SELECT rowid FROM find_misses ORDER BY at
                     LIMIT max((SELECT count(*) FROM find_misses) - ?1, 0))",
            )
            .db()?
            .execute([as_i64(FIND_MISSES_CAP)])
            .db()?;
            Ok(())
        })
    }

    fn put_function_test(&self, test: &FunctionTest) -> Result<bool> {
        let (total, covered) = crate::functions::check_test(test)?;
        self.write("put_function_test", |c| {
            let n = exec(
                c,
                L::FunctionTestPut,
                rusqlite::params![
                    &test.fn_hash,
                    &test.case,
                    &test.test_json,
                    &test.signed_by,
                    &test.last_run_at,
                    &test.last_result,
                    &test.last_detail,
                    total,
                    covered
                ],
            )?;
            if n == 1 {
                let results = crate::functions::result_counts(test.last_result.as_deref());
                bump_counter(
                    c,
                    &test.fn_hash,
                    "",
                    1,
                    results,
                    test.last_run_at.as_deref(),
                    (total.into(), covered.into()),
                )?;
            }
            Ok(n == 1)
        })
    }

    fn function_test(&self, fn_hash: &str, case: &str) -> Result<Option<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        crate::functions::check_case(case)?;
        self.run("function_test", |c| {
            let mut st = c.prepare_cached(L::FunctionTestOne.sql()).db()?;
            let mut rows = st.query_map(rusqlite::params![fn_hash, case], function_test_row).db()?;
            rows.next().transpose().db()
        })
    }

    fn function_tests_failing(&self, fn_hash: &str, fn_name: Option<&str>, limit: usize) -> Result<Vec<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("function_tests_failing", |c| {
            let mut st = c.prepare_cached(L::FunctionTestsFailing.sql()).db()?;
            let rows = st.query_map(rusqlite::params![fn_hash, fn_name, limit], function_test_row).db()?;
            rows.collect::<rusqlite::Result<Vec<FunctionTest>>>().db()
        })
    }

    fn false_test_cells(&self, fn_hash: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("false_test_cells", |c| {
            let mut st = c.prepare_cached(L::FalseTestCells.sql()).db()?;
            let rows = st.query_map(rusqlite::params![fn_hash, limit], |r| Ok((r.get(0)?, r.get(1)?))).db()?;
            rows.collect::<rusqlite::Result<Vec<(String, Vec<u8>)>>>().db()
        })
    }

    fn dependants_of(&self, name: &str, at: &str, limit: usize) -> Result<Vec<(String, String)>> {
        check_function_name(name, "name")?;
        check_instant(at, "at")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("dependants_of", |c| {
            let mut st = c.prepare_cached(L::DependantsOf.sql()).db()?;
            let rows = st.query_map(rusqlite::params![name, at, limit], |r| Ok((r.get(0)?, r.get(1)?))).db()?;
            rows.collect::<rusqlite::Result<Vec<(String, String)>>>().db()
        })
    }

    fn catalog_entries_all(&self, limit: usize) -> Result<Vec<(CatalogKind, String, String, String)>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("catalog_entries_all", |c| {
            let mut st = c.prepare_cached(L::CatalogEntriesAll.sql()).db()?;
            let rows = st.query_map([limit], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).db()?;
            rows.map(|row| {
                let (kind, name, label, description): (String, String, String, String) = row.db()?;
                let kind =
                    CatalogKind::parse(&kind).ok_or_else(|| invalid("catalog", "a stored entry has a bad kind"))?;
                Ok((kind, name, label, description))
            })
            .collect()
        })
    }

    fn dictionary_values_ordered(&self, owner: &str, key: &str, limit: usize) -> Result<Vec<String>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("dictionary_values_ordered", |c| {
            let mut st = c.prepare_cached(L::DictionaryValuesOrdered.sql()).db()?;
            let rows = st.query_map(rusqlite::params![owner, key, limit], |r| r.get::<_, String>(0)).db()?;
            rows.collect::<rusqlite::Result<Vec<String>>>().db()
        })
    }

    fn function_tests(&self, fn_hash: &str) -> Result<Vec<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("function_tests", |c| {
            let mut st = c.prepare_cached(L::FunctionTestsOf.sql()).db()?;
            let rows = st.query_map([fn_hash], function_test_row).db()?;
            rows.collect::<rusqlite::Result<Vec<FunctionTest>>>().db()
        })
    }

    fn record_function_test(
        &self,
        fn_hash: &str,
        case: &str,
        run_at: &str,
        result: &str,
        detail: &str,
    ) -> Result<bool> {
        check_hex(fn_hash, 64, "fn_hash")?;
        crate::functions::check_case(case)?;
        check_instant(run_at, "run_at")?;
        crate::functions::check_result(result)?;
        self.write("record_function_test", |c| {
            let Some((old_name, old_result)) = function_test_old(c, fn_hash, case)? else { return Ok(false) };
            let n = exec(c, L::FunctionTestRecord, rusqlite::params![run_at, result, detail, fn_hash, case])?;
            if n == 1 {
                // The test moves within its row: the row's counts change by the difference.
                let (before, after) = (
                    crate::functions::result_counts(old_result.as_deref()),
                    crate::functions::result_counts(Some(result)),
                );
                let row = old_name.unwrap_or_default();
                let d = [after[0] - before[0], after[1] - before[1], after[2] - before[2], after[3] - before[3]];
                bump_counter(c, fn_hash, &row, 0, d, None, (0, 0))?;
                exec(c, L::CounterRunRecompute, rusqlite::params![fn_hash, row])?;
            }
            Ok(n == 1)
        })
    }

    fn set_function_branches(&self, fn_hash: &str, total: u32, covered: u32) -> Result<()> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let (total, covered) = crate::functions::branch_counts(total, covered)?;
        self.write("set_function_branches", |c| {
            exec(c, L::FunctionBranches, rusqlite::params![fn_hash, total, covered]).map(|_| ())
        })
    }

    fn function_readers(&self, name: &str) -> Result<Vec<String>> {
        check_function_name(name, "name")?;
        self.run("function_readers", |c| get_strings(c, L::FunctionReaders, name))
    }

    fn bound_names(&self, fn_hash: &str) -> Result<Vec<String>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("bound_names", |c| get_strings(c, L::BoundNames, fn_hash))
    }

    fn name_function_tests(&self, fn_hash: &str, names: &[(String, String)]) -> Result<u64> {
        check_hex(fn_hash, 64, "fn_hash")?;
        for (case, name) in names {
            crate::functions::check_case(case)?;
            check_function_name(name, "name")?;
        }
        if names.is_empty() {
            return Ok(0);
        }
        self.write("name_function_tests", |c| {
            let mut named = 0u64;
            // The rows moved from unnamed to their names, with their results: [tests, pass,
            // fail, error, not_run] per name (0.5.3: the counters move with them).
            let mut moved: std::collections::BTreeMap<String, [i64; 5]> = std::collections::BTreeMap::new();
            for (case, name) in names {
                let Some((old_name, result)) = function_test_old(c, fn_hash, case)? else { continue };
                if old_name.is_some() {
                    continue;
                }
                if exec(c, L::FunctionTestName, rusqlite::params![fn_hash, case, name])? == 1 {
                    named += 1;
                    let d = crate::functions::result_counts(result.as_deref());
                    let e = moved.entry(name.clone()).or_insert([0; 5]);
                    e[0] += 1;
                    for (i, x) in d.iter().enumerate() {
                        e[i + 1] += x;
                    }
                }
            }
            for (name, d) in &moved {
                bump_counter(c, fn_hash, name, d[0], [d[1], d[2], d[3], d[4]], None, (0, 0))?;
                bump_counter(c, fn_hash, "", -d[0], [-d[1], -d[2], -d[3], -d[4]], None, (0, 0))?;
                exec(c, L::CounterRunRecompute, rusqlite::params![fn_hash, name])?;
            }
            if !moved.is_empty() {
                exec(c, L::CounterRunRecompute, rusqlite::params![fn_hash, ""])?;
            }
            Ok(named)
        })
    }

    fn unnamed_function_tests(&self, limit: usize) -> Result<Vec<FunctionTest>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("unnamed_function_tests", |c| {
            let mut st = c.prepare_cached(L::FunctionTestsUnnamed.sql()).db()?;
            let rows = st.query_map([limit], function_test_row).db()?;
            rows.collect::<rusqlite::Result<Vec<FunctionTest>>>().db()
        })
    }

    fn function_test_summary(&self, fn_hash: &str, fn_name: Option<&str>) -> Result<TestSummary> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("function_test_summary", |c| {
            let mut st = c.prepare_cached(L::FunctionTestSummary.sql()).db()?;
            let (count, pass, fail, error, not_run, last_run_at, total, covered) = st
                .query_row(rusqlite::params![fn_hash, fn_name], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ))
                })
                .db()?;
            // The branch counts are integer with a check of at least zero: an `as` is exact.
            Ok(TestSummary {
                count: count as u64,
                pass: pass as u64,
                fail: fail as u64,
                error: error as u64,
                not_run: not_run as u64,
                last_run_at,
                branches_total: total as u32,
                branches_covered: covered as u32,
            })
        })
    }

    fn function_tests_page(
        &self,
        fn_hash: &str,
        fn_name: Option<&str>,
        after_case: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("function_tests_page", |c| {
            let rows: Vec<FunctionTest> = match fn_name {
                None => {
                    let mut st = c.prepare_cached(L::FunctionTestsPage.sql()).db()?;
                    let mapped = st.query_map(rusqlite::params![fn_hash, after_case, limit], function_test_row).db()?;
                    mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
                }
                Some(name) => {
                    let mut st = c.prepare_cached(L::FunctionTestsPageNamed.sql()).db()?;
                    let mapped =
                        st.query_map(rusqlite::params![fn_hash, name, after_case, limit], function_test_row).db()?;
                    mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
                }
            };
            Ok(rows)
        })
    }

    fn stored_receipts_of(&self, ns: &str, fn_name: &str, fn_hash: &str, limit: usize) -> Result<Vec<Vec<u8>>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("stored_receipts_of", |c| {
            let mut st = c.prepare_cached(L::StoredReceiptsOf.sql()).db()?;
            let rows = st.query_map(rusqlite::params![fn_hash, ns, fn_name, limit], |r| r.get::<_, Vec<u8>>(0)).db()?;
            rows.collect::<rusqlite::Result<Vec<Vec<u8>>>>().db()
        })
    }

    fn slice_values_page(
        &self,
        slice: &Slice,
        asof: &str,
        after_cell_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SliceValue>> {
        check_instant(asof, "asof")?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let v = slice.to_value();
        if version_slice(slice, &v)?.is_some() {
            // A version slice is one cell's versions: paged here, from its values.
            let mut values = self.slice_values(slice, asof)?;
            values.retain(|x| after_cell_id.is_none_or(|a| x.version.cell_id.as_str() > a));
            values.sort_by(|a, b| a.version.cell_id.cmp(&b.version.cell_id));
            values.truncate(limit);
            return Ok(values);
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let p = Pattern::of(&v)?;
        self.run("slice_values_page", |c| values_page(&p, c, asof, after_cell_id, limit))
    }

    fn slice_page(&self, slice: &Slice, asof: &str, after: Option<&[u8]>, limit: usize) -> Result<Vec<SliceValue>> {
        check_instant(asof, "asof")?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let v = slice.to_value();
        if version_slice(slice, &v)?.is_some() {
            return Err(Error::Unsupported("a version slice has no key order to page by".into()));
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let p = Pattern::of(&v)?;
        self.run("slice_page", |c| slice_page_rows(&p, c, asof, after, limit))
    }

    fn slice_values_limit(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<SliceValue>> {
        check_instant(asof, "asof")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.run("slice_values_limit", |c| {
                // The statement of slice_values, with its LIMIT: the same order (tx).
                let sql = format!("{} LIMIT ?5", L::VersionsBetweenValues.sql());
                let mut st = c.prepare(&sql).db()?;
                let mut rows = st.query(rusqlite::params![cell_id, from, to, asof, limit]).db()?;
                let mut out = Vec::new();
                while let Some(r) = rows.next().db()? {
                    out.push(value_of(value_columns(r).db()?)?.0);
                }
                Ok(out)
            });
        }
        let p = Pattern::of(&v)?;
        self.run("slice_values_limit", |c| values_limited(&p, c, asof, limit))
    }

    fn versions_limit(&self, cell_id: &str, from: &str, to: &str, limit: usize) -> Result<Vec<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(from, "from")?;
        check_instant(to, "to")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("versions_limit", |c| {
            let mut st = c.prepare_cached(L::VersionRangeLimit.sql()).db()?;
            let rows = st.query_map(rusqlite::params![cell_id, from, to, limit], |r| version_row(r, cell_id)).db()?;
            rows.collect::<rusqlite::Result<Vec<Version>>>().db()
        })
    }

    fn stored_cell_count(&self, ns: &str, fn_name: &str, fn_hash: &str) -> Result<u64> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("stored_cell_count", |c| {
            let n: i64 = c
                .prepare_cached(L::StoredCellCount.sql())
                .db()?
                .query_row(rusqlite::params![fn_hash, ns, fn_name], |r| r.get(0))
                .db()?;
            Ok(n as u64)
        })
    }

    fn has_key_value(&self, owner: &str, key: &str, value: &str) -> Result<bool> {
        self.run("has_key_value", |c| {
            c.prepare_cached(L::DictionaryHas.sql())
                .db()?
                .query_row(rusqlite::params![owner, key, value], |r| r.get(0))
                .db()
        })
    }

    fn key_value_count(&self, owner: &str, key: &str) -> Result<u64> {
        self.run("key_value_count", |c| {
            let n: i64 = c
                .prepare_cached(L::DictionaryCount.sql())
                .db()?
                .query_row(rusqlite::params![owner, key], |r| r.get(0))
                .db()?;
            Ok(n as u64)
        })
    }

    fn mark_key_open(&self, owner: &str, key: &str) -> Result<()> {
        if owner.is_empty() || owner.len() > 512 || key.is_empty() || key.len() > 128 {
            return Err(invalid("key", "an owner of 1 to 512 bytes and a key of 1 to 128 bytes"));
        }
        self.write("mark_key_open", |c| exec(c, L::KeyOpenPut, rusqlite::params![owner, key]).map(|_| ()))
    }

    fn open_key_count(&self) -> Result<u64> {
        self.run("open_key_count", |c| {
            let n: i64 = c.prepare_cached(L::OpenKeyCount.sql()).db()?.query_row([], |r| r.get(0)).db()?;
            Ok(n as u64)
        })
    }

    fn key_is_open(&self, owner: &str, key: &str) -> Result<bool> {
        self.run("key_is_open", |c| {
            c.prepare_cached(L::KeyIsOpen.sql()).db()?.query_row(rusqlite::params![owner, key], |r| r.get(0)).db()
        })
    }

    fn catalog_entry(&self, name: &str) -> Result<Option<CatalogEntry>> {
        self.run("catalog_entry", |c| {
            let mut st = c.prepare_cached(L::CatalogEntryNamed.sql()).db()?;
            let row = st
                .query_row([name], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                    ))
                })
                .optional()
                .db()?;
            let Some((kind, name, hash, label, description, args, examples, source, signed_by)) = row else {
                return Ok(None);
            };
            entry_from_columns(&kind, name, hash, label, description, &args, &examples, &source, signed_by).map(Some)
        })
    }

    fn catalog_entry_ext(&self, kind: CatalogKind, name: &str) -> Result<Option<CatalogEntryExt>> {
        self.run("catalog_entry_ext", |c| {
            let mut st = c.prepare_cached(CATALOG_EXT_SELECT_BY_KIND_NAME).db()?;
            st.query_row(rusqlite::params![kind.as_str(), name], ext_from_row).optional().db()?.transpose()
        })
    }

    fn catalog_entries_with_facets(&self) -> Result<Vec<FacetedEntry>> {
        self.run("catalog_entries_with_facets", |c| {
            let mut st = c
                .prepare(
                    "SELECT e.kind, e.name, e.hash, e.facets,
                            CASE WHEN e.kind = 'class'
                                 THEN (SELECT min(b.tx) FROM class_bindings b WHERE b.class_name = e.name)
                                 ELSE (SELECT min(b.tx) FROM name_bindings b WHERE b.fn_name = e.name) END,
                            e.source_name
                       FROM catalog_entries e
                      WHERE e.facets <> '{}'
                      ORDER BY e.kind, e.name",
                )
                .db()?;
            type FacetRow = (String, String, Option<String>, String, Option<String>, Option<String>);
            let rows: Vec<FacetRow> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .db()?
                .collect::<rusqlite::Result<_>>()
                .db()?;
            rows.into_iter()
                .map(|(kind, name, hash, facets, first_bound, source_name)| {
                    Ok(FacetedEntry {
                        kind: CatalogKind::parse(&kind)
                            .ok_or_else(|| invalid("catalog", "a stored entry has a bad kind"))?,
                        name,
                        hash,
                        facets: parse_facets(&facets)?,
                        first_bound,
                        source_name,
                    })
                })
                .collect()
        })
    }

    fn extensions_of(&self, class: &str) -> Result<Vec<CatalogEntryExt>> {
        check_function_name(class, "class")?;
        self.run("extensions_of", |c| {
            let mut st = c
                .prepare_cached(
                    "SELECT kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name
                       FROM catalog_entries WHERE extends = ?1 ORDER BY name",
                )
                .db()?;
            let rows = st.query_map([class], ext_from_row).db()?.collect::<rusqlite::Result<Vec<_>>>().db()?;
            rows.into_iter().collect()
        })
    }

    fn extensions_of_limit(&self, class: &str, limit: usize) -> Result<Vec<CatalogEntryExt>> {
        check_function_name(class, "class")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("extensions_of", |c| {
            let mut st = c
                .prepare_cached(
                    "SELECT kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name
                       FROM catalog_entries WHERE extends = ?1 ORDER BY name LIMIT ?2",
                )
                .db()?;
            let rows = st
                .query_map(rusqlite::params![class, limit], ext_from_row)
                .db()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .db()?;
            rows.into_iter().collect()
        })
    }

    fn catalog_summary(&self) -> Result<Summary> {
        self.run("catalog_summary", |c| {
            let rows: Vec<(String, String, String, i64)> = {
                let mut st = c
                    .prepare(
                        "SELECT kind, name, label, CASE WHEN kind = 'function'
                             THEN (SELECT COALESCE(sum(x.n), 0) FROM cell_counts x WHERE x.fn_hash = e.hash) ELSE 0 END
                           FROM catalog_entries e ORDER BY kind, name",
                    )
                    .db()?;
                let mapped = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).db()?;
                mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
            };
            let mut entries = Vec::with_capacity(rows.len());
            for (kind, name, label, n_cells) in rows {
                let kind = CatalogKind::parse(&kind).ok_or_else(|| invalid("catalog", "a stored entry has a bad kind"))?;
                entries.push(SummaryEntry { kind, name, label, n_cells });
            }
            let groups: Vec<(String, String, i64)> = {
                let mut st = c.prepare("SELECT owner, key, count(*) FROM key_values GROUP BY owner, key ORDER BY owner, key").db()?;
                let mapped = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).db()?;
                mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
            };
            let mut dictionaries = Vec::with_capacity(groups.len());
            for (owner, key, n_values) in groups {
                let values: Vec<DictionaryValue> = {
                    let mut st = c
                        .prepare("SELECT value, label, n_cells FROM key_values WHERE owner = ?1 AND key = ?2 ORDER BY value LIMIT ?3")
                        .db()?;
                    let mapped = st
                        .query_map(rusqlite::params![owner, key, as_i64(SUMMARY_VALUES)], |r| {
                            Ok(DictionaryValue { value: r.get(0)?, label: r.get(1)?, n_cells: r.get(2)? })
                        })
                        .db()?;
                    mapped.collect::<rusqlite::Result<Vec<_>>>().db()?
                };
                dictionaries.push(Dictionary { owner, key, n_values, values });
            }
            Ok(Summary { entries, dictionaries })
        })
    }

    // ---- Listings (0.7.0; CONTRACT-http-listing L3) ----------------------------------------------

    fn put_listing(&self, l: &Listing) -> Result<ListingWrite> {
        check_listing(l)?;
        let d = FunctionDescriptor::from_json(&l.descriptor)?;
        let canonical = d.canonical();
        let subjects = subjects_json(&l.subjects);
        let facets = listing_facets_json(&l.subjects);
        self.write("put_listing", |c| {
            let stored = select_listing(c, &l.name)?;
            if stored.as_ref().is_some_and(|s| same_listing(s, l)) {
                return Ok(ListingWrite::Unchanged);
            }
            // The descriptor (once) and its inputs, as put_descriptor writes them.
            exec(c, L::DescriptorPut, rusqlite::params![l.fn_hash, canonical])?;
            for input in &d.inputs {
                exec(c, L::DescriptorInputPut, rusqlite::params![l.fn_hash, input])?;
            }
            // The row: listed_at is kept by a change.
            c.prepare_cached(
                "INSERT INTO listings (name, host, fn_hash, subjects, descriptor, key_id, label, description, fetched_at, listed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (name) DO UPDATE SET host = excluded.host, fn_hash = excluded.fn_hash,
                     subjects = excluded.subjects, descriptor = excluded.descriptor, key_id = excluded.key_id,
                     label = excluded.label, description = excluded.description, fetched_at = excluded.fetched_at",
            )
            .db()?
            .execute(rusqlite::params![
                l.name,
                l.host,
                l.fn_hash,
                subjects,
                l.descriptor,
                l.key_id,
                l.label,
                l.description,
                l.fetched_at,
                l.listed_at
            ])
            .db()?;
            // The catalogue entry (source listed; SQLite keeps no embeddings).
            c.prepare_cached(
                "INSERT INTO catalog_entries (kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name, updated_at)
                 VALUES ('function', ?1, ?2, ?3, ?4, '{}', '[]', 'listed', ?5, ?6, NULL, NULL, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                 ON CONFLICT (kind, name) DO UPDATE SET hash = excluded.hash, label = excluded.label,
                     description = excluded.description, args = excluded.args, examples = excluded.examples,
                     source = excluded.source, signed_by = excluded.signed_by, facets = excluded.facets,
                     extends = NULL, source_name = NULL, updated_at = excluded.updated_at",
            )
            .db()?
            .execute(rusqlite::params![l.name, l.fn_hash, l.label, l.description, l.host, facets])
            .db()?;
            Ok(if stored.is_some() { ListingWrite::Changed } else { ListingWrite::Inserted })
        })
    }

    fn listing(&self, name: &str) -> Result<Option<Listing>> {
        self.run("listing", |c| select_listing(c, name))
    }

    fn listings_of(&self, host: &str) -> Result<Vec<Listing>> {
        self.run("listings_of", |c| {
            let sql = format!("SELECT {LISTING_COLUMNS} FROM listings WHERE host = ?1 ORDER BY name");
            let mut st = c.prepare_cached(&sql).db()?;
            let mut rows = st.query([host]).db()?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().db()? {
                out.push(listing_from_sqlite(r)?);
            }
            Ok(out)
        })
    }

    fn listing_count(&self) -> Result<u64> {
        self.run("listing_count", |c| {
            let n: i64 = c.query_row("SELECT count(*) FROM listings", [], |r| r.get(0)).db()?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
    }

    fn delete_listing(&self, name: &str) -> Result<bool> {
        self.write("delete_listing", |c| {
            c.prepare_cached("DELETE FROM catalog_entries WHERE kind = 'function' AND source = 'listed' AND name = ?1")
                .db()?
                .execute([name])
                .db()?;
            let removed = c.prepare_cached("DELETE FROM listings WHERE name = ?1").db()?.execute([name]).db()?;
            Ok(removed > 0)
        })
    }

    fn put_listed_host(&self, h: &ListedHost) -> Result<bool> {
        check_listed_host(h)?;
        self.write("put_listed_host", |c| {
            if listed_host_row(c, &h.host)?.as_ref() == Some(h) {
                return Ok(false);
            }
            c.prepare_cached(
                "INSERT INTO listed_hosts (host, last_seen, unreachable_since) VALUES (?1, ?2, ?3)
                 ON CONFLICT (host) DO UPDATE SET last_seen = excluded.last_seen,
                     unreachable_since = excluded.unreachable_since",
            )
            .db()?
            .execute(rusqlite::params![h.host, h.last_seen, h.unreachable_since])
            .db()?;
            Ok(true)
        })
    }

    fn listed_host(&self, host: &str) -> Result<Option<ListedHost>> {
        self.run("listed_host", |c| listed_host_row(c, host))
    }

    fn listed_hosts(&self) -> Result<Vec<ListedHost>> {
        self.run("listed_hosts", |c| {
            let mut st =
                c.prepare_cached("SELECT host, last_seen, unreachable_since FROM listed_hosts ORDER BY host").db()?;
            let mut rows = st.query([]).db()?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().db()? {
                out.push(listed_host_sqlite(r)?);
            }
            Ok(out)
        })
    }

    fn delete_listed_host(&self, host: &str) -> Result<bool> {
        self.write("delete_listed_host", |c| {
            c.prepare_cached(
                "DELETE FROM catalog_entries WHERE kind = 'function' AND source = 'listed'
                 AND name IN (SELECT name FROM listings WHERE host = ?1)",
            )
            .db()?
            .execute([host])
            .db()?;
            c.prepare_cached("DELETE FROM listings WHERE host = ?1").db()?.execute([host]).db()?;
            let gone = c.prepare_cached("DELETE FROM listed_hosts WHERE host = ?1").db()?.execute([host]).db()?;
            Ok(gone > 0)
        })
    }
}

/// The columns of a value row: (tx, receipt_hash, asof, value_type, value, cell_id,
/// arguments, ns, fn, fn_hash, sort_key).
type ValueColumns =
    (String, String, String, String, String, String, String, String, String, String, String, Vec<u8>, i64);

fn value_columns(r: &rusqlite::Row<'_>) -> rusqlite::Result<ValueColumns> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
        r.get(11)?,
        r.get::<_, Option<i64>>("seq")?.unwrap_or(0),
    ))
}

/// A member's value and cell, rebuilt from the typed-argument index (with its arguments).
fn value_of(c: ValueColumns) -> Result<(SliceValue, Args)> {
    let (tx, receipt_hash, asof, recorded, ty, value, cell_id, args, ns, fn_name, fn_hash, sort_key, seq) = c;
    let ty = Type::from_name(&ty).ok_or_else(|| bad("type"))?;
    let args = args_of(&args)?;
    let cell = Cell { args: args.clone(), fn_hash, fn_name, ns };
    Ok((
        SliceValue {
            // A slice's values leave the withdrawn versions out (the query does).
            version: Version { cell_id, tx, receipt_hash, asof, withdrawn: false, recorded, seq },
            value: Scalar::from_lexical(ty, &value)?,
            cell,
            sort_key,
        },
        args,
    ))
}

/// The constraints of a slice as the queries take them (`:cons`): `[name, kind, type, lo,
/// hi]`, a number range's ends as sort keys, a bound value's lexical form as `lo`.
fn constraints(p: &Pattern) -> String {
    let items: Vec<Value> = p
        .args
        .iter()
        .map(|a| {
            let (lo, hi) = match a.kind {
                Kind::Range => (sort_key(&a.ty, &a.lo), sort_key(&a.ty, &a.hi)),
                _ => (a.lo.clone(), a.hi.clone()),
            };
            Value::Array(
                [a.name.clone(), a.kind.as_str().to_string(), a.ty.clone(), lo, hi]
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            )
        })
        .collect();
    String::from_utf8(Value::Array(items).to_jcs()).unwrap_or_default()
}

/// Binds the named parameters a statement has, from `params` (skipping the others).
fn bind(st: &mut rusqlite::CachedStatement<'_>, params: &[(&str, SqlValue)]) -> Result<()> {
    for (name, value) in params {
        if let Some(i) = st.parameter_index(name).db()? {
            st.raw_bind_parameter(i, value).db()?;
        }
    }
    Ok(())
}

/// The bound or ranged argument holding the fewest cells (probed, up to 10,001, when there
/// are several, or when `count` asks), else none (every cell of the function); and the
/// number of candidates when probed.
fn drive<'p>(p: &'p Pattern, c: &Connection, count: bool) -> Result<(Option<&'p Constraint>, Option<i64>)> {
    let probe = |a: Option<&Constraint>| -> Result<i64> {
        match a {
            None => c.prepare_cached(L::ProbeCells.sql()).db()?.query_row([&p.fn_hash], |r| r.get(0)).db(),
            Some(a) => {
                let (lo, hi) = (sort_key(&a.ty, &a.lo), sort_key(&a.ty, &a.hi));
                c.prepare_cached(L::SliceProbe.sql())
                    .db()?
                    .query_row(rusqlite::params![p.fn_hash, a.name, a.ty, lo, hi], |r| r.get(0))
                    .db()
            }
        }
    };
    let constrained: Vec<&Constraint> = p.args.iter().filter(|a| a.kind != Kind::Open).collect();
    Ok(match constrained.as_slice() {
        [] => (None, if count { Some(probe(None)?) } else { None }),
        [one] => (Some(*one), if count { Some(probe(Some(one))?) } else { None }),
        several => {
            let mut best: Option<(i64, &Constraint)> = None;
            for a in several {
                let n = probe(Some(a))?;
                if best.is_none_or(|(m, _)| n < m) {
                    best = Some((n, a));
                }
            }
            (best.map(|(_, a)| a), best.map(|(n, _)| n))
        }
    })
}

/// The parameters of a slice query: the slice's, and the driving argument's range.
fn params(p: &Pattern, asof: &str, drive: Option<&Constraint>) -> Vec<(&'static str, SqlValue)> {
    let text = |s: &str| SqlValue::Text(s.to_string());
    let mut out = vec![
        (":fn_hash", text(&p.fn_hash)),
        (":ns", text(&p.ns)),
        (":fn", text(&p.fn_name)),
        (":nargs", SqlValue::Integer(p.args.len() as i64)),
        (":asof", text(asof)),
        (":cons", SqlValue::Text(constraints(p))),
    ];
    if let Some(a) = drive {
        out.push((":d_name", text(&a.name)));
        out.push((":d_type", text(&a.ty)));
        out.push((":d_lo", SqlValue::Text(sort_key(&a.ty, &a.lo))));
        out.push((":d_hi", SqlValue::Text(sort_key(&a.ty, &a.hi))));
    }
    out
}

/// The latest withdrawal among a slice's members as of `asof`: the (tx, asof) of the greatest,
/// one indexed query (0.6.0; SPEC 8.2).
fn withdrawn_latest(p: &Pattern, c: &Connection, asof: &str) -> Result<Option<(String, String)>> {
    let (d, _) = drive(p, c, false)?;
    let q = if d.is_some() { L::SliceWithdrawnDrive } else { L::SliceWithdrawnAll };
    let mut st = c.prepare_cached(q.sql()).db()?;
    bind(&mut st, &params(p, asof, d))?;
    let mut rows = st.raw_query();
    match rows.next().db()? {
        Some(r) => Ok(Some((r.get::<_, String>(0).db()?, r.get::<_, String>(1).db()?))),
        None => Ok(None),
    }
}

/// The rows of one of the member queries `qs` (all cells, driven), each made by `make`.
fn rows<T>(
    p: &Pattern,
    c: &Connection,
    asof: &str,
    qs: [L; 2],
    make: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let (d, _) = drive(p, c, false)?;
    let q = if d.is_some() { qs[1] } else { qs[0] };
    let mut st = c.prepare_cached(q.sql()).db()?;
    bind(&mut st, &params(p, asof, d))?;
    let mut rows = st.raw_query();
    let mut out = Vec::new();
    while let Some(r) = rows.next().db()? {
        out.push(make(r).db()?);
    }
    Ok(out)
}

/// The key of a member: the "type:value" of its open and ranged arguments, by name.
fn key_of(p: &Pattern, args: &Args) -> Vec<String> {
    p.args
        .iter()
        .filter(|a| a.kind != Kind::Value)
        .filter_map(|a| args.get(&a.name).and_then(Arg::scalar))
        .map(|s| format!("{}:{}", s.ty().as_str(), s.lexical()))
        .collect()
}

/// Members with their argument lists (JSON), in key order (then by cell id).
fn in_key_order<T>(p: &Pattern, rows: Vec<(T, String)>, cell_of: impl Fn(&T) -> &String) -> Result<Vec<T>> {
    let mut keyed = Vec::with_capacity(rows.len());
    for (m, args) in rows {
        keyed.push((key_of(p, &args_of(&args)?), m));
    }
    keyed.sort_by(|(a, ma), (b, mb)| cmp_keys(a, b).then_with(|| cell_of(ma).cmp(cell_of(mb))));
    Ok(keyed.into_iter().map(|(_, m)| m).collect())
}

/// Members with their parsed arguments, in key order (then by cell id).
fn in_key_order_parsed<T>(p: &Pattern, rows: Vec<(T, Args)>, cell_of: impl Fn(&T) -> &String) -> Result<Vec<T>> {
    let mut keyed: Vec<(Vec<String>, T)> = rows.into_iter().map(|(m, args)| (key_of(p, &args), m)).collect();
    keyed.sort_by(|(a, ma), (b, mb)| cmp_keys(a, b).then_with(|| cell_of(ma).cmp(cell_of(mb))));
    Ok(keyed.into_iter().map(|(_, m)| m).collect())
}

/// The members' values of a slice of cells as of `asof`, in key order, at most `limit`: the
/// statement of `slice_values` with the key order and the LIMIT in SQL. The order of `cmp_keys`
/// is, per key argument by name, the type's name, then the sort key (which sorts as the value
/// does, a number's as the decimal does).
/// The members' values as of `asof` in cell id order, after `after` (exclusive), at most
/// `limit`: the statement of the members with the cell id order and the LIMIT in SQL (0.5.3).
fn values_page(p: &Pattern, c: &Connection, asof: &str, after: Option<&str>, limit: i64) -> Result<Vec<SliceValue>> {
    let (d, _) = drive(p, c, false)?;
    let base = if d.is_some() { L::SliceValuesDrive } else { L::SliceValuesAll };
    let sql = format!("{} AND (:after IS NULL OR c.cell_id > :after) ORDER BY c.cell_id LIMIT :limit", base.sql());
    let mut binds: Vec<(String, SqlValue)> = params(p, asof, d).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    binds.push((":after".to_string(), after.map_or(SqlValue::Null, |a| SqlValue::Text(a.to_string()))));
    binds.push((":limit".to_string(), SqlValue::Integer(limit)));
    let mut st = c.prepare(&sql).db()?;
    for (name, value) in &binds {
        if let Some(i) = st.parameter_index(name).db()? {
            st.raw_bind_parameter(i, value).db()?;
        }
    }
    let mut rows = st.raw_query();
    let mut out = Vec::new();
    while let Some(r) = rows.next().db()? {
        out.push(value_of(value_columns(r).db()?)?.0);
    }
    Ok(out)
}

/// A page of the members of a slice of cells in sort-key order (CONTRACT-0.5.1 S2), after
/// `after` (exclusive), at most `limit`. The candidates are the sort-key range of the slice's
/// leading arguments when there is one (`slices::key_bounds`: O(limit) rows); else those of its
/// most selective bound argument, ordered by key (O(members)); else the function's index walked
/// from its start. Withdrawn and absent members are left out before the LIMIT counts them.
fn slice_page_rows(
    p: &Pattern,
    c: &Connection,
    asof: &str,
    after: Option<&[u8]>,
    limit: i64,
) -> Result<Vec<SliceValue>> {
    let bounds = crate::slices::key_bounds(&p.args);
    let d = if bounds.is_some() { None } else { drive(p, c, false)?.0 };
    let mut sql = if d.is_some() { L::SliceValuesDrive.sql().to_string() } else { L::SliceKeyBase.sql().to_string() };
    let mut binds: Vec<(String, SqlValue)> = params(p, asof, d).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    if let Some((lo, hi)) = &bounds {
        sql += " AND c.sort_key >= :lo";
        binds.push((":lo".to_string(), SqlValue::Blob(lo.clone())));
        if let Some(hi) = hi {
            sql += " AND c.sort_key < :hi";
            binds.push((":hi".to_string(), SqlValue::Blob(hi.clone())));
        }
    }
    if let Some(a) = after {
        sql += " AND c.sort_key > :after";
        binds.push((":after".to_string(), SqlValue::Blob(a.to_vec())));
    }
    sql += " ORDER BY c.sort_key LIMIT :limit";
    binds.push((":limit".to_string(), SqlValue::Integer(limit)));
    let mut st = c.prepare(&sql).db()?;
    for (name, value) in &binds {
        if let Some(i) = st.parameter_index(name).db()? {
            st.raw_bind_parameter(i, value).db()?;
        }
    }
    let mut rows = st.raw_query();
    let mut out = Vec::new();
    while let Some(r) = rows.next().db()? {
        out.push(value_of(value_columns(r).db()?)?.0);
    }
    Ok(out)
}

fn values_limited(p: &Pattern, c: &Connection, asof: &str, limit: i64) -> Result<Vec<SliceValue>> {
    let (d, _) = drive(p, c, false)?;
    let base = if d.is_some() { L::SliceValuesDrive } else { L::SliceValuesAll };
    let keys: Vec<&Constraint> = p.args.iter().filter(|a| a.kind != Kind::Value).collect();
    let mut order = Vec::with_capacity(keys.len() * 2 + 1);
    for i in 0..keys.len() {
        let of =
            |col: &str| format!("(SELECT x.{col} FROM cell_args x WHERE x.cell_id = c.cell_id AND x.name = :k{i})");
        order.push(of("type"));
        order.push(of("sort_key"));
    }
    order.push("c.cell_id".to_string());
    let sql = format!("{} ORDER BY {} LIMIT :limit", base.sql(), order.join(", "));
    let mut binds: Vec<(String, SqlValue)> = params(p, asof, d).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    for (i, a) in keys.iter().enumerate() {
        binds.push((format!(":k{i}"), SqlValue::Text(a.name.clone())));
    }
    binds.push((":limit".to_string(), SqlValue::Integer(limit)));
    let mut st = c.prepare(&sql).db()?;
    for (name, value) in &binds {
        if let Some(i) = st.parameter_index(name).db()? {
            st.raw_bind_parameter(i, value).db()?;
        }
    }
    let mut rows = st.raw_query();
    let mut out = Vec::new();
    while let Some(r) = rows.next().db()? {
        out.push(value_of(value_columns(r).db()?)?.0);
    }
    Ok(out)
}

/// Whether a matching cell has a stored version answering from after `asof`: from
/// whichever side is smaller (both probed, up to 10,001), the function's receipts with an
/// asof after `asof`, or the slice's candidate cells.
fn changed_after(p: &Pattern, c: &Connection, asof: &str) -> Result<bool> {
    let (d, candidates) = drive(p, c, true)?;
    let recent: i64 = c
        .prepare_cached(L::ProbeRecent.sql())
        .db()?
        .query_row(rusqlite::params![p.fn_hash, asof], |r| r.get(0))
        .db()?;
    let q = if recent <= candidates.unwrap_or(i64::MAX) {
        L::ChangedRecent
    } else if d.is_some() {
        L::ChangedCandDrive
    } else {
        L::ChangedCandAll
    };
    let d = if q == L::ChangedCandDrive { d } else { None };
    let mut st = c.prepare_cached(q.sql()).db()?;
    bind(&mut st, &params(p, asof, d))?;
    let mut rows = st.raw_query();
    Ok(rows.next().db()?.is_some())
}

/// A text search's entry hit as its columns: id, kind, name, hash, label, description, args,
/// examples, source, signed_by, and the total score (0.5.3).
type HitRow = (i64, String, String, Option<String>, String, String, String, String, String, Option<String>, i64);

/// The `AND (...)` prefilter of a text search over `fields` (SQL expressions): one clause per
/// word the SQL can match exactly (ASCII, no `"`, `\` or control characters: SQLite's LIKE
/// folds ASCII only, and JSON text escapes those), with its parameters numbered from ?1.
fn like_where_sqlite(words: &Words, fields: &[&str], first: usize) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut params = Vec::new();
    for i in 0..words.len() {
        if let Some(p) = words.pattern(i, sqlite_safe) {
            params.push(p);
            let n = first + params.len() - 1;
            let ors: Vec<String> = fields.iter().map(|f| format!("{f} LIKE ?{n} ESCAPE '!'")).collect();
            sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
        }
    }
    (sql, params)
}

fn sqlite_safe(word: &str) -> bool {
    word.is_ascii() && !word.chars().any(|c| c == '"' || c == '\\' || c.is_control())
}

fn unsupported_vectors() -> Error {
    Error::Unsupported("vectors need PostgreSQL with the vector extension; SQLite has none".into())
}

/// A function test row of `function_tests`, in the column order of `FunctionTestsOf`.
fn function_test_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FunctionTest> {
    Ok(FunctionTest {
        fn_hash: r.get(0)?,
        case: r.get(1)?,
        test_json: r.get(2)?,
        signed_by: r.get(3)?,
        last_run_at: r.get(4)?,
        last_result: r.get(5)?,
        last_detail: r.get(6)?,
        branches_total: r.get(7)?,
        branches_covered: r.get(8)?,
    })
}

/// The columns of a catalogue entry with its facets and `extends`, by kind and name.
const CATALOG_EXT_SELECT_BY_KIND_NAME: &str =
    "SELECT kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name
       FROM catalog_entries WHERE kind = ?1 AND name = ?2";

/// A catalogue entry with its facets, `extends` and `source_name` from a row with the twelve
/// columns of [`CATALOG_EXT_SELECT_BY_KIND_NAME`] (or of any query with those columns in that order).
fn ext_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<CatalogEntryExt>> {
    let entry = entry_from_columns(
        &r.get::<_, String>(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        &r.get::<_, String>(5)?,
        &r.get::<_, String>(6)?,
        &r.get::<_, String>(7)?,
        r.get(8)?,
    );
    let facets: String = r.get(9)?;
    let extends: Option<String> = r.get(10)?;
    let source_name: Option<String> = r.get(11)?;
    Ok(entry.and_then(|entry| entry_ext(entry, &facets, extends, source_name)))
}

// ---- Listings (0.7.0) --------------------------------------------------------------------------

/// The columns of a listing, in the order [`listing_from_sqlite`] reads them.
const LISTING_COLUMNS: &str =
    "name, host, fn_hash, subjects, descriptor, key_id, label, description, fetched_at, listed_at";

fn listing_from_sqlite(r: &rusqlite::Row<'_>) -> Result<Listing> {
    let subjects: String = r.get(3).db()?;
    Ok(Listing {
        name: r.get(0).db()?,
        host: r.get(1).db()?,
        fn_hash: r.get(2).db()?,
        subjects: parse_subjects(&subjects)?,
        descriptor: r.get(4).db()?,
        key_id: r.get(5).db()?,
        label: r.get(6).db()?,
        description: r.get(7).db()?,
        fetched_at: r.get(8).db()?,
        listed_at: r.get(9).db()?,
    })
}

fn select_listing(c: &Connection, name: &str) -> Result<Option<Listing>> {
    let sql = format!("SELECT {LISTING_COLUMNS} FROM listings WHERE name = ?1");
    let mut st = c.prepare_cached(&sql).db()?;
    let mut rows = st.query([name]).db()?;
    match rows.next().db()? {
        Some(r) => listing_from_sqlite(r).map(Some),
        None => Ok(None),
    }
}

fn listed_host_sqlite(r: &rusqlite::Row<'_>) -> Result<ListedHost> {
    Ok(ListedHost { host: r.get(0).db()?, last_seen: r.get(1).db()?, unreachable_since: r.get(2).db()? })
}

fn listed_host_row(c: &Connection, host: &str) -> Result<Option<ListedHost>> {
    let mut st =
        c.prepare_cached("SELECT host, last_seen, unreachable_since FROM listed_hosts WHERE host = ?1").db()?;
    let mut rows = st.query([host]).db()?;
    match rows.next().db()? {
        Some(r) => listed_host_sqlite(r).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slices::cmp_decimal;

    #[test]
    fn decimal_keys_sort_as_the_numbers() {
        let mut numbers: Vec<String> = [
            "0", "9", "10", "-1.5", "-1", "-1.25", "-1.11", "-1.1", "-0.1", "-0.099", "0.099", "0.1", "0.109", "0.11",
            "1", "1.1", "1.109", "1.11", "9.5", "99.999", "100", "-100", "-10", "-9", "123", "-123", "0.0001",
            "-0.0001", "1000000", "-1000000", "12.5", "12.05", "-12.5", "-12.05", "120", "-120", "1.2", "-1.2",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        numbers.push("9".repeat(128));
        numbers.push(format!("-{}", "9".repeat(127)));
        numbers.push(format!("1{}", "0".repeat(127)));
        numbers.push(format!("-1{}", "0".repeat(126)));
        numbers.push(format!("0.{}1", "0".repeat(125)));
        numbers.push(format!("-0.{}1", "0".repeat(124)));
        numbers.push(format!("{}.{}", "1".repeat(64), "1".repeat(63)));
        numbers.push(format!("-{}.{}", "1".repeat(63), "1".repeat(63)));
        for a in &numbers {
            assert!(a.len() <= 128, "{a}");
            for b in &numbers {
                assert_eq!(decimal_key(a).cmp(&decimal_key(b)), cmp_decimal(a, b), "{a} vs {b}");
            }
        }
        // Equal numbers, equal keys; the forms.
        assert_eq!(decimal_key("0"), "1");
        assert_eq!(decimal_key("100"), "25031");
        assert_eq!(decimal_key("0.0012"), "249812");
        assert_eq!(decimal_key("-1.5"), "049884~");
        assert_eq!(decimal_key(&"9".repeat(128)), format!("2628{}", "9".repeat(128)));
    }

    #[test]
    fn result_codes_have_names() {
        assert_eq!(result_code_name(5), "SQLITE_BUSY");
        assert_eq!(result_code_name(1811), "SQLITE_CONSTRAINT_TRIGGER");
        assert_eq!(result_code_name(14), "SQLITE_CANTOPEN");
        assert_eq!(result_code_name(10 | (99 << 8)), "SQLITE_IOERR(25354)");
    }

    #[test]
    fn sqlite_urls_name_paths() {
        assert_eq!(sqlite_path("sqlite:store.db"), "store.db");
        assert_eq!(sqlite_path("sqlite://store.db"), "store.db");
        assert_eq!(sqlite_path("sqlite:///var/lib/x.db"), "/var/lib/x.db");
        assert_eq!(sqlite_path("/var/lib/x.db"), "/var/lib/x.db");
        assert_eq!(sqlite_path("x.db"), "x.db");
    }

    /// The plan of a statement, one line per step (`EXPLAIN QUERY PLAN`).
    fn plan(c: &Connection, sql: &str) -> Vec<String> {
        let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = st.parameter_count();
        for i in 1..=n {
            st.raw_bind_parameter(i, "x").unwrap();
        }
        let mut rows = st.raw_query();
        let mut out = Vec::new();
        while let Some(r) = rows.next().unwrap() {
            out.push(r.get::<_, String>(3).unwrap());
        }
        out
    }

    /// The statements of bounded slices and of the probes look tables up by index only: no
    /// step scans a table or a whole index (SQLite's counterpart of the PostgreSQL tests'
    /// sequential-scan counters). Planned on the migrated schema with statistics of a
    /// loaded store's shape (100,000 cells of one function).
    #[test]
    fn bounded_slices_use_indexes() {
        let c = Connection::open_in_memory().unwrap();
        for m in crate::migrate::sqlite::MIGRATIONS {
            c.execute_batch(m.sql).unwrap();
        }
        // Statistics as ANALYZE leaves them for 100,000 cells, 2 arguments each.
        c.execute_batch(
            "ANALYZE sqlite_schema;
             INSERT INTO sqlite_stat1 (tbl, idx, stat) VALUES
               ('cells', 'cells_fn_hash', '100000 100000 1'),
               ('cells', 'sqlite_autoindex_cells_1', '100000 1'),
               ('cell_args', 'cell_args_key', '200000 200000 100000 100000 1 1'),
               ('cell_args', 'sqlite_autoindex_cell_args_1', '200000 2 1'),
               ('versions', 'sqlite_autoindex_versions_1', '100000 1 1'),
               ('versions', 'versions_receipt_hash', '100000 1'),
               ('receipts', 'sqlite_autoindex_receipts_1', '100000 1'),
               ('receipts', 'receipts_cell_tx', '100000 1 1'),
               ('receipts', 'receipts_fn_hash_asof', '100000 100000 1'),
               ('receipts', 'receipts_cell_asof', '100000 1 1');
             ANALYZE sqlite_schema;",
        )
        .unwrap();
        // A scan of the constraints (json_each) or of a subquery's rows is fine; of a table
        // or an index, not.
        let scans = |s: &str| s.starts_with("SCAN ") && !s.starts_with("SCAN k") && !s.starts_with("SCAN (subquery");
        for q in [
            L::SliceMembersDrive,
            L::SliceValuesDrive,
            L::ChangedCandDrive,
            L::ChangedRecent,
            L::SliceProbe,
            L::ProbeRecent,
            L::ProbeCells,
            L::VersionsBetween,
            L::VersionsBetweenValues,
            L::StoredAsof,
            L::ComputedAsof,
            L::CellDependents,
        ] {
            let steps = plan(&c, q.sql());
            assert!(!steps.iter().any(|s| scans(s)), "{q:?} scans:\n{steps:#?}");
        }
        // The unbounded ones walk the function's cells by index, and nothing else.
        for q in [L::SliceMembersAll, L::SliceValuesAll, L::ChangedCandAll] {
            let steps = plan(&c, q.sql());
            assert!(
                steps.iter().any(|s| s.contains("SEARCH c USING") && s.contains("cells_fn_hash")),
                "{q:?}: {steps:#?}"
            );
            assert!(!steps.iter().any(|s| scans(s)), "{q:?} scans:\n{steps:#?}");
        }
    }
}
