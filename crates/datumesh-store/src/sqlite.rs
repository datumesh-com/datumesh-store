//! `SqliteStore`: the store in one SQLite file (since 0.4.0).

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::{Db, Error, Result};
use crate::slices::{check_entry, cmp_keys, is_hash, is_instant, version_slice, Constraint, Kind, Pattern};
use crate::sqlite_sql::L;
use crate::{Binding, Namespace, SliceMember, SliceValue, Store, Version};
use datumesh_core::json::{self, Value};
use datumesh_core::lexical::{check_dns_name, check_function_name, check_hex, check_instant};
use datumesh_core::{
    keys_from_response, sha256_hex, verify, Arg, Args, Cell, ErrorCode, FunctionDescriptor, Keys, Manifest, Receipt,
    Resolver, Scalar, Slice, Type,
};

/// How a [`SqliteStore`] opens and uses its connections.
///
/// Builder style: `SqliteOptions::new().busy_timeout(Duration::from_secs(5))`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqliteOptions {
    max_size: usize,
    busy_timeout: Duration,
    durable: bool,
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
        SqliteOptions { max_size: 8, busy_timeout: Duration::from_secs(30), durable: true }
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
                .query_row(rusqlite::params![cell_id, asof], |r| Ok((version_row(r, cell_id)?, r.get(3)?)))
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

fn version_row(r: &rusqlite::Row<'_>, cell_id: &str) -> rusqlite::Result<Version> {
    Ok(Version { cell_id: cell_id.to_string(), tx: r.get(0)?, receipt_hash: r.get(1)?, asof: r.get(2)? })
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
            match Receipt::from_json(&bytes) {
                Ok(r) => Ok(Some(r.value().clone())),
                Err(e) => {
                    tracing::error!(target: "datumesh_store", cell_id = %cell.id(), error = %e, "stored receipt does not parse");
                    Ok(None)
                }
            }
        })
    }
}

impl Store for SqliteStore {
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String> {
        let d = FunctionDescriptor::from_json(bytes)?;
        let hash = d.hash();
        let canonical = d.canonical();
        self.run("put_descriptor", |c| {
            if exists(c, L::DescriptorExists, &hash)? {
                return Ok(());
            }
            self.locked_write(c, |c| exec(c, L::DescriptorPut, rusqlite::params![hash, canonical]).map(|_| ()))?;
            self.bound_wal(c);
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_hash = %hash, "put_descriptor");
        Ok(hash)
    }

    fn put_receipt(&self, bytes: &[u8]) -> Result<String> {
        // Steps 1-4 first: a receipt that is not one is refused without a query.
        let r = Receipt::from_json(bytes)?;
        let canonical = json::parse(bytes)?.to_jcs();
        let hash = r.hash().to_string();
        let cell = r.cell();
        let cell_id = cell.id();
        let host = r.signer_host().to_string();
        let (mut entries, mut inputs): (Vec<String>, Vec<String>) =
            r.inputs().iter().cloned().partition(|e| e.starts_with("slice:"));
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
            if let Some(doc) = get_bytes(c, L::NamespaceGet, &host)? {
                keys.insert_response(&host, 200, Some(&doc));
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
                let value = r.value();
                exec(
                    c,
                    L::ReceiptPut,
                    rusqlite::params![
                        hash,
                        canonical,
                        cell_id,
                        r.tx(),
                        cell.fn_hash,
                        r.publisher(),
                        r.asof(),
                        value.ty().as_str(),
                        value.lexical()
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
            let meta: Option<(String, String)> = c
                .prepare_cached(L::ReceiptMeta.sql())
                .db()?
                .query_row([receipt_hash], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .db()?;
            let Some((r_cell, r_tx)) = meta else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            if r_cell != cell_id {
                return Err(Error::Mismatch { what: "cell_id", expected: cell_id.to_string(), found: r_cell });
            }
            if r_tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: r_tx });
            }
            if let Some(l) = get_strings(c, L::VersionLatestTx, cell_id)?.pop() {
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "version",
                        key: cell_id.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            exec(c, L::VersionPut, rusqlite::params![cell_id, tx, receipt_hash])?;
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
        self.run("namespace", |c| {
            c.prepare_cached(L::NamespaceGet.sql())
                .db()?
                .query_row([host], |r| {
                    Ok(Namespace {
                        host: host.to_string(),
                        keydoc: r.get(0)?,
                        validated_at: r.get(1)?,
                        suspended_at: r.get(2)?,
                    })
                })
                .optional()
                .db()
        })
    }

    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()> {
        check_function_name(fn_name, "fn_name")?;
        check_hex(fn_hash, 64, "fn_hash")?;
        check_instant(tx, "tx")?;
        self.write("bind_name", |c| {
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
            c.prepare_cached(L::NameAsof.sql()).db()?.query_row([fn_name, asof], |r| r.get(0)).optional().db()
        })
    }

    fn resolve_binding(&self, fn_name: &str, asof: &str) -> Result<Option<Binding>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_binding", |c| {
            c.prepare_cached(L::BindingAsof.sql())
                .db()?
                .query_row([fn_name, asof], |r| Ok(Binding { fn_hash: r.get(0)?, tx: r.get(1)? }))
                .optional()
                .db()
        })
    }

    fn put_computed(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(tx, "tx")?;
        check_hex(receipt_hash, 64, "receipt_hash")?;
        let asof = self.write("put_computed", |c| {
            let meta: Option<(String, String, String)> = c
                .prepare_cached(L::ReceiptMeta.sql())
                .db()?
                .query_row([receipt_hash], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()
                .db()?;
            let Some((r_cell, r_tx, asof)) = meta else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
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
                let cell_id: String = r.get(4)?;
                Ok((
                    SliceMember {
                        version: Version { cell_id, tx: r.get(0)?, receipt_hash: r.get(1)?, asof: r.get(2)? },
                        receipt: r.get(3)?,
                    },
                    r.get::<_, String>(5)?,
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
                    Ok(SliceMember { version: version_row(r, cell_id)?, receipt: r.get(3)? })
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
}

/// The columns of a value row: (tx, receipt_hash, asof, value_type, value, cell_id,
/// arguments, ns, fn, fn_hash).
type ValueColumns = (String, String, String, String, String, String, String, String, String, String);

fn value_columns(r: &rusqlite::Row<'_>) -> rusqlite::Result<ValueColumns> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

/// A member's value and cell, rebuilt from the typed-argument index (with its arguments).
fn value_of(c: ValueColumns) -> Result<(SliceValue, Args)> {
    let (tx, receipt_hash, asof, ty, value, cell_id, args, ns, fn_name, fn_hash) = c;
    let ty = Type::from_name(&ty).ok_or_else(|| bad("type"))?;
    let args = args_of(&args)?;
    let cell = Cell { args: args.clone(), fn_hash, fn_name, ns };
    Ok((
        SliceValue {
            version: Version { cell_id, tx, receipt_hash, asof },
            value: Scalar::from_lexical(ty, &value)?,
            cell,
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
