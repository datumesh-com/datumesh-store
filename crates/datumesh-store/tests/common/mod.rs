//! Helpers shared by the integration tests (included with `#[macro_use] mod common;`).
//!
//! The conformance suite runs every test of the shared files against both backends: a test
//! is a function taking a [`Backend`], and `backends!(name, ...)` makes `pg::name` and
//! `sqlite::name` of it. [`TestDb`] is a database of its own for either backend (a
//! PostgreSQL database created from `DATABASE_URL` and dropped afterwards, or a SQLite file
//! in a directory of its own, removed afterwards), so tests run in parallel without sharing
//! state. [`PgDb`] is the PostgreSQL one alone, for the Postgres-only tests (TLS, the pool,
//! `pg_unstable`, the backfills of migrations 2-4).
#![allow(dead_code)]

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use datumesh_core::json::{self, Value};
use datumesh_core::{Cell, Evaluation, Keys, Receipt, ReceiptBuilder, Resolver, Scalar, SigningKey, Type};
use datumesh_store::pg_unstable::postgres::{Client, Config, NoTls};
use datumesh_store::{migrate, pg_unstable, PgStore, PoolOptions, SqliteOptions, SqliteStore, Store};

/// `backends!(a, b)`: `#[test] fn a()` in modules `pg` and `sqlite`, each calling `a` with
/// its backend.
#[allow(unused_macros)]
macro_rules! backends {
    ($($name:ident),* $(,)?) => {
        mod pg {
            $(#[test] fn $name() { super::$name(crate::common::Backend::Pg) })*
        }
        mod sqlite {
            $(#[test] fn $name() { super::$name(crate::common::Backend::Sqlite) })*
        }
    };
}

/// A store backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Pg,
    Sqlite,
}

pub fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL names a PostgreSQL 16 database (see README)")
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_name(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    format!("{prefix}_{}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst), nanos)
}

/// `url` with `param` (`key=value`) appended to its query.
pub fn with_param(url: &str, param: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}{param}")
}

/// The URL of database `name` on the server of `admin` (same credentials and parameters).
pub fn db_url(admin: &str, name: &str) -> String {
    let (path, query) = match admin.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (admin, None),
    };
    let (base, _) = path.rsplit_once('/').unwrap();
    match query {
        Some(q) => format!("{base}/{name}?{q}"),
        None => format!("{base}/{name}"),
    }
}

// ---- The store under test --------------------------------------------------------------

/// A store of either backend, as the engine holds one: shared, behind a trait object.
/// Derefs to `dyn Store`; a `Resolver` itself (delegating), for the core's kit and `verify`.
#[derive(Clone)]
pub struct TStore(pub Arc<dyn Store + Send + Sync>);

impl std::ops::Deref for TStore {
    type Target = dyn Store + Send + Sync;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl Resolver for TStore {
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        self.0.descriptor(fn_hash)
    }
    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        self.0.receipt(hash)
    }
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<Scalar> {
        self.0.evaluate(cell, asof)
    }
    fn manifest(&self, entry: &str) -> Option<Cow<'_, [u8]>> {
        self.0.manifest(entry)
    }
    fn evaluation(&self, cell: &Cell, asof: &str) -> Option<Evaluation> {
        self.0.evaluation(cell, asof)
    }
}

/// Anything the helpers below can store through: a [`TStore`], either concrete store, or
/// an `Arc` of one.
pub trait AsStore {
    fn st(&self) -> &dyn Store;
}

impl AsStore for TStore {
    fn st(&self) -> &dyn Store {
        &*self.0
    }
}

impl AsStore for PgStore {
    fn st(&self) -> &dyn Store {
        self
    }
}

impl AsStore for SqliteStore {
    fn st(&self) -> &dyn Store {
        self
    }
}

impl AsStore for dyn Store + '_ {
    fn st(&self) -> &dyn Store {
        self
    }
}

impl AsStore for dyn Store + Send + Sync + '_ {
    fn st(&self) -> &dyn Store {
        self
    }
}

impl<T: AsStore + ?Sized> AsStore for Arc<T> {
    fn st(&self) -> &dyn Store {
        (**self).st()
    }
}

// ---- A database of either backend -------------------------------------------------------

/// A database of its own for `backend`, dropped (or removed) when the value is dropped.
pub struct TestDb {
    pub backend: Backend,
    pg: Option<PgDb>,
    lite: Option<SqliteDb>,
}

impl TestDb {
    /// An empty database (no migrations applied).
    pub fn empty(backend: Backend) -> TestDb {
        match backend {
            Backend::Pg => TestDb { backend, pg: Some(PgDb::empty()), lite: None },
            Backend::Sqlite => TestDb { backend, pg: None, lite: Some(SqliteDb::empty()) },
        }
    }

    /// A database with every migration of its backend applied.
    pub fn migrated(backend: Backend) -> TestDb {
        match backend {
            Backend::Pg => TestDb { backend, pg: Some(PgDb::migrated()), lite: None },
            Backend::Sqlite => TestDb { backend, pg: None, lite: Some(SqliteDb::migrated()) },
        }
    }

    pub fn is_pg(&self) -> bool {
        self.backend == Backend::Pg
    }

    /// The PostgreSQL database (panics on SQLite).
    pub fn pg(&self) -> &PgDb {
        self.pg.as_ref().expect("a PostgreSQL test database")
    }

    /// The SQLite database (panics on PostgreSQL).
    pub fn lite(&self) -> &SqliteDb {
        self.lite.as_ref().expect("a SQLite test database")
    }

    /// `pg` on PostgreSQL, `sqlite` on SQLite: a statement in each dialect.
    pub fn pick<'a>(&self, pg: &'a str, sqlite: &'a str) -> &'a str {
        if self.is_pg() {
            pg
        } else {
            sqlite
        }
    }

    /// A store holding the key documents of rates.example.org and calc.example.net (those
    /// of the vectors), so that their receipts can be stored.
    pub fn store(&self) -> TStore {
        let s = self.bare_store();
        put_vector_keys(&s, &vector("receipt/stored-number"));
        s
    }

    /// `store`, with a pool of `n` connections.
    pub fn store_sized(&self, n: usize) -> TStore {
        let s = self.bare_store_sized(n);
        put_vector_keys(&s, &vector("receipt/stored-number"));
        s
    }

    /// A store without key documents.
    pub fn bare_store(&self) -> TStore {
        self.bare_store_sized(8)
    }

    /// A store without key documents, with a pool of `n` connections.
    pub fn bare_store_sized(&self, n: usize) -> TStore {
        match self.backend {
            Backend::Pg => TStore(Arc::new(self.pg().bare_store_with(PoolOptions::new().max_size(n)))),
            Backend::Sqlite => TStore(Arc::new(self.lite().store_with(SqliteOptions::new().max_size(n)))),
        }
    }

    /// A store for loading many rows fast: commits not flushed to disk one by one
    /// (PostgreSQL `synchronous_commit = off`, SQLite `synchronous = NORMAL`), as the
    /// benchmark loads.
    pub fn loader_store(&self, n: usize) -> TStore {
        let s = match self.backend {
            Backend::Pg => {
                let config = self.pg().config.clone();
                TStore(Arc::new(
                    pg_unstable::with_connector(
                        move || {
                            let mut c = config.connect(NoTls)?;
                            c.batch_execute("SET synchronous_commit = off")?;
                            Ok(c)
                        },
                        PoolOptions::new().max_size(n),
                    )
                    .unwrap(),
                ))
            }
            Backend::Sqlite => {
                TStore(Arc::new(self.lite().store_with(SqliteOptions::new().max_size(n).durable(false))))
            }
        };
        put_vector_keys(&s, &vector("receipt/stored-number"));
        s
    }

    /// Another store of this database, opened by its URL or path (its own pool).
    pub fn open_again(&self) -> TStore {
        match self.backend {
            Backend::Pg => TStore(Arc::new(PgStore::connect(&self.pg().url()).unwrap())),
            Backend::Sqlite => TStore(Arc::new(SqliteStore::open(&self.lite().path).unwrap())),
        }
    }

    /// The URL (PostgreSQL) or path (SQLite) of this database.
    pub fn url(&self) -> String {
        match self.backend {
            Backend::Pg => self.pg().url(),
            Backend::Sqlite => self.lite().path.to_str().unwrap().to_string(),
        }
    }

    /// Runs SQL (one or more statements) on a raw connection; the database's message on
    /// failure.
    pub fn exec(&self, sql: &str) -> Result<(), String> {
        match self.backend {
            Backend::Pg => self.pg().client().batch_execute(sql).map_err(|e| pg_message(&e)),
            Backend::Sqlite => self.lite().conn().execute_batch(sql).map_err(|e| e.to_string()),
        }
    }

    /// Runs SQL that must fail as an append-only table refuses it (the message says so).
    pub fn refused(&self, sql: &str) -> String {
        let msg = self.exec(sql).expect_err(sql);
        assert!(msg.contains("append-only"), "{sql}: {msg}");
        msg
    }

    /// The rows of a query on a raw connection, each column as text.
    pub fn texts(&self, sql: &str) -> Vec<Vec<String>> {
        match self.backend {
            Backend::Pg => {
                let rows = self.pg().client().query(sql, &[]).unwrap();
                rows.iter().map(|r| (0..r.len()).map(|i| r.get::<_, String>(i)).collect()).collect()
            }
            Backend::Sqlite => {
                let c = self.lite().conn();
                let mut st = c.prepare(sql).unwrap();
                let n = st.column_count();
                st.query_map([], |r| (0..n).map(|i| r.get::<_, String>(i)).collect::<rusqlite::Result<Vec<String>>>())
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect()
            }
        }
    }

    /// One integer from a query on a raw connection.
    pub fn int(&self, sql: &str) -> i64 {
        match self.backend {
            Backend::Pg => self.pg().client().query_one(sql, &[]).unwrap().get(0),
            Backend::Sqlite => self.lite().conn().query_row(sql, [], |r| r.get(0)).unwrap(),
        }
    }

    /// The number of rows of a table.
    pub fn count(&self, table: &str) -> i64 {
        self.int(&format!("SELECT count(*) FROM {table}"))
    }

    /// Updates the planner's statistics (`ANALYZE`).
    pub fn analyze(&self) {
        self.exec("ANALYZE").unwrap();
    }

    /// Inserts a receipt row with raw SQL, bypassing the store: `bytes` under `hash`, with
    /// the columns a writer gives (the derived ones from the bytes, as the database checks).
    pub fn plant_receipt(&self, hash: &str, bytes: &[u8], r: &Receipt) {
        match self.backend {
            Backend::Pg => {
                self.pg()
                    .client()
                    .execute(
                        "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, fn_hash, publisher)
                         VALUES ($1, $2, $3, $4, $4::text::timestamptz, $5, $6)",
                        &[&hash, &bytes, &r.cell().id(), &r.tx(), &r.cell().fn_hash, &r.publisher()],
                    )
                    .unwrap();
            }
            Backend::Sqlite => {
                self.lite()
                    .conn()
                    .execute(
                        "INSERT INTO receipts (hash, bytes, cell_id, tx, asof, fn_hash, publisher, value_type, value)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        rusqlite::params![
                            hash,
                            bytes,
                            r.cell().id(),
                            r.tx(),
                            r.asof(),
                            r.cell().fn_hash,
                            r.publisher(),
                            r.value().ty().as_str(),
                            r.value().lexical()
                        ],
                    )
                    .unwrap();
            }
        }
    }

    /// A raw insert of a version row (no store checks); the database's message on failure.
    pub fn raw_version(&self, cell: &str, tx: &str, receipt_hash: &str) -> Result<(), String> {
        match self.backend {
            Backend::Pg => self
                .pg()
                .client()
                .execute(
                    "INSERT INTO versions (cell_id, tx, tx_at, receipt_hash) VALUES ($1, $2, $2::text::timestamptz, $3)",
                    &[&cell, &tx, &receipt_hash],
                )
                .map(|_| ())
                .map_err(|e| pg_message(&e)),
            Backend::Sqlite => self
                .lite()
                .conn()
                .execute(
                    "INSERT INTO versions (cell_id, tx, receipt_hash) VALUES (?1, ?2, ?3)",
                    rusqlite::params![cell, tx, receipt_hash],
                )
                .map(|_| ())
                .map_err(|e| e.to_string()),
        }
    }

    /// A raw insert of a computed version row (no store checks).
    pub fn raw_computed(&self, cell: &str, tx: &str, asof: &str, receipt_hash: &str) -> Result<(), String> {
        match self.backend {
            Backend::Pg => self
                .pg()
                .client()
                .execute(
                    "INSERT INTO computed_versions (cell_id, tx, tx_at, asof, asof_at, receipt_hash)
                     VALUES ($1, $2, $2::text::timestamptz, $3, $3::text::timestamptz, $4)",
                    &[&cell, &tx, &asof, &receipt_hash],
                )
                .map(|_| ())
                .map_err(|e| pg_message(&e)),
            Backend::Sqlite => self
                .lite()
                .conn()
                .execute(
                    "INSERT INTO computed_versions (cell_id, tx, asof, receipt_hash) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![cell, tx, asof, receipt_hash],
                )
                .map(|_| ())
                .map_err(|e| e.to_string()),
        }
    }
}

/// The server's message of a PostgreSQL error.
pub fn pg_message(e: &datumesh_store::pg_unstable::postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_string()).unwrap_or_else(|| e.to_string())
}

/// The number of rows of a table.
pub fn count(db: &TestDb, table: &str) -> i64 {
    db.count(table)
}

// ---- PostgreSQL ------------------------------------------------------------------------

/// A PostgreSQL database of its own, dropped when the value is dropped.
pub struct PgDb {
    pub name: String,
    /// The database's configuration, for plain-text clients (`NoTls`; `sslmode=prefer`
    /// then means no TLS).
    pub config: Config,
    admin: String,
}

impl PgDb {
    /// An empty database (no migrations applied).
    pub fn empty() -> PgDb {
        PgDb::empty_on(&admin_url())
    }

    /// An empty database on the server of `admin` (a URL whose role may create databases).
    pub fn empty_on(admin: &str) -> PgDb {
        let name = unique_name("dm_test");
        let admin_config: Config = admin.parse().unwrap();
        let mut c = admin_config.connect(NoTls).unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let mut config = admin_config.clone();
        config.dbname(&name);
        PgDb { name, config, admin: admin.to_string() }
    }

    /// A database with every migration applied (through a store, as `migrate::apply` takes).
    pub fn migrated() -> PgDb {
        let db = PgDb::empty();
        migrate::apply(&db.bare_store()).unwrap();
        db
    }

    /// A plain-text client of this database (for checks in SQL).
    pub fn client(&self) -> Client {
        self.config.connect(NoTls).unwrap()
    }

    /// A store holding the key documents of rates.example.org and calc.example.net (those
    /// of the vectors), so that their receipts can be stored.
    pub fn store(&self) -> PgStore {
        self.store_with(PoolOptions::new())
    }

    pub fn store_with(&self, options: PoolOptions) -> PgStore {
        let s = self.bare_store_with(options);
        put_vector_keys(&s, &vector("receipt/stored-number"));
        s
    }

    /// A store without key documents.
    pub fn bare_store(&self) -> PgStore {
        self.bare_store_with(PoolOptions::new())
    }

    /// A store without key documents, connected with `PgStore::connect_with` (the URL).
    pub fn bare_store_with(&self, options: PoolOptions) -> PgStore {
        PgStore::connect_with(&self.url(), options).unwrap()
    }

    /// A store without key documents whose connections come from
    /// `pg_unstable::with_connector` (plain-text clients of this database).
    pub fn connector_store_with(&self, options: PoolOptions) -> PgStore {
        let config = self.config.clone();
        pg_unstable::with_connector(move || config.connect(NoTls), options).unwrap()
    }

    /// `postgres://…/<this database>` for `PgStore::connect`.
    pub fn url(&self) -> String {
        db_url(&self.admin, &self.name)
    }
}

impl Drop for PgDb {
    fn drop(&mut self) {
        let admin: Config = self.admin.parse().unwrap();
        if let Ok(mut c) = admin.connect(NoTls) {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name));
        }
    }
}

// ---- SQLite ----------------------------------------------------------------------------

/// A SQLite database file in a directory of its own (under the target's temporary
/// directory), removed with the directory when the value is dropped.
pub struct SqliteDb {
    pub dir: PathBuf,
    pub path: PathBuf,
}

impl SqliteDb {
    /// No file yet: opening a store creates it, empty.
    pub fn empty() -> SqliteDb {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sqlite-tests").join(unique_name("db"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        SqliteDb { dir, path }
    }

    /// A database with every SQLite migration applied.
    pub fn migrated() -> SqliteDb {
        let db = SqliteDb::empty();
        migrate::sqlite::apply(&db.store_with(SqliteOptions::new())).unwrap();
        db
    }

    pub fn store_with(&self, options: SqliteOptions) -> SqliteStore {
        SqliteStore::open_with(&self.path, options).unwrap()
    }

    /// A raw connection of this database (as another program would open it: no pragmas
    /// beyond a busy timeout, foreign keys not enforced).
    pub fn conn(&self) -> rusqlite::Connection {
        let c = rusqlite::Connection::open(&self.path).unwrap();
        c.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
        c
    }
}

impl Drop for SqliteDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- Vectors and receipts --------------------------------------------------------------

pub fn spec(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec").join(rel)
}

pub fn vectors(group: &str) -> Vec<Value> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(spec("vectors").join(group))
        .expect("spec submodule present")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files.iter().map(|f| json::parse(&std::fs::read(f).unwrap()).unwrap()).collect()
}

pub fn vector(id: &str) -> Value {
    let (group, _) = id.split_once('/').unwrap();
    vectors(group).into_iter().find(|v| str_of(v, "id") == id).unwrap_or_else(|| panic!("no vector {id}"))
}

pub fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

pub fn hex(h: &str) -> Vec<u8> {
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect()
}

pub fn keys(v: &Value) -> Keys {
    let mut out = Keys::new();
    for (host, env) in v.get("keys").and_then(Value::as_object).unwrap_or(&[]) {
        let https = env.get("https").unwrap();
        match https.get("status").and_then(Value::as_number_text) {
            Some(n) => {
                let body = https.get("body").and_then(Value::as_str).map(str::as_bytes);
                out.insert_response(host, n.parse().unwrap(), body);
            }
            _ => out.insert_unavailable(host),
        }
    }
    out
}

/// The key document body a vector offers for `host`.
pub fn key_doc(v: &Value, host: &str) -> Vec<u8> {
    let env = v.get("keys").and_then(|k| k.get(host)).unwrap();
    str_of(env.get("https").unwrap(), "body").as_bytes().to_vec()
}

/// The vector's receipt input as bytes.
pub fn input(v: &Value) -> Vec<u8> {
    str_of(v, "input").as_bytes().to_vec()
}

/// JCS of JSON text.
pub fn jcs(bytes: &[u8]) -> Vec<u8> {
    json::parse(bytes).unwrap().to_jcs()
}

/// The canonical JSON text of a whole receipt (payload plus `sig`).
pub fn receipt_json(r: &Receipt) -> Vec<u8> {
    let mut members = vec![("sig".to_string(), Value::String(r.sig().to_string()))];
    members.extend(json::parse(r.payload()).unwrap().as_object().unwrap().iter().cloned());
    Value::Object(members).to_jcs()
}

/// The signing key of a vector with a `keypair`.
pub fn signing_key(v: &Value) -> SigningKey {
    let s: [u8; 32] = hex(str_of(v.get("keypair").unwrap(), "seed")).try_into().unwrap();
    SigningKey::from_seed(&s)
}

/// A new version of the stored-number cell (EUR/USD fx_rate): same cell, the given value,
/// recorded at `tx` and answered as of `tx`, signed with the vector's key.
pub fn stored_number_at(value: &str, tx: &str) -> Vec<u8> {
    stored_number_with(value, tx, tx)
}

/// A receipt of the stored-number cell (EUR/USD fx_rate) with the given value, recorded at
/// `tx`, answered as of `asof` (`tx <= asof`: later is a late-signed receipt) and issued at
/// `asof`, signed with the vector's key.
pub fn stored_number_with(value: &str, tx: &str, asof: &str) -> Vec<u8> {
    let v = vector("receipt/stored-number");
    let base = Receipt::from_json(&input(&v)).unwrap();
    let r = ReceiptBuilder::new()
        .cell(base.cell().clone())
        .value(Scalar::from_lexical(Type::Number, value).unwrap())
        .valid(base.valid().clone())
        .asof(asof)
        .tx(tx)
        .issued(asof)
        .publisher(base.publisher())
        .signer_host(base.signer_host())
        .licence(base.licence())
        .summary(format!("1 EUR was worth {value} USD at the 15 March 2024 reference rate."))
        .sign(&signing_key(&v))
        .unwrap();
    receipt_json(&r)
}

/// A receipt of the computed-with-lineage cell (convert 100 EUR to USD) whose inputs are
/// `inputs`, recorded at `tx`, as of `asof`, issued at `asof`, signed with the vector's key
/// (calc.example.net).
pub fn lineage_with(inputs: &[&str], value: &str, tx: &str, asof: &str) -> Vec<u8> {
    let v = vector("receipt/computed-with-lineage");
    let base = Receipt::from_json(&input(&v)).unwrap();
    let r = ReceiptBuilder::new()
        .cell(base.cell().clone())
        .value(Scalar::from_lexical(Type::Number, value).unwrap())
        .valid(base.valid().clone())
        .asof(asof)
        .tx(tx)
        .issued(asof)
        .publisher(base.publisher())
        .signer_host(base.signer_host())
        .inputs(inputs.iter().map(|s| s.to_string()))
        .licence(base.licence())
        .summary("100 EUR in USD.")
        .sign(&signing_key(&v))
        .unwrap();
    receipt_json(&r)
}

/// The descriptor of the stored-number vector (fx_rate, stored), canonical.
pub fn fx_rate_descriptor() -> Vec<u8> {
    vector("receipt/stored-number").get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs()
}

/// When the key documents used by the tests were validated.
pub const VALIDATED_AT: &str = "2024-01-01T00:00:00.000000Z";

/// Stores every key document a vector offers (a 200 response with a body) as its host's
/// namespace.
pub fn put_vector_keys(s: &(impl AsStore + ?Sized), v: &Value) {
    let s = s.st();
    for (host, env) in v.get("keys").and_then(Value::as_object).unwrap_or(&[]) {
        let https = env.get("https").unwrap();
        if https.get("status").and_then(Value::as_number_text) != Some("200") {
            continue;
        }
        if let Some(body) = https.get("body").and_then(Value::as_str) {
            s.put_namespace(host, body.as_bytes(), VALIDATED_AT).unwrap();
        }
    }
}

// ---- Slices (0.3.1) ----------------------------------------------------------------------

/// The host signing the slice tests' stored receipts.
pub const SLICE_HOST: &str = "slices.example.org";

/// The slice tests' signing key (a fixed test seed).
pub fn slice_key() -> SigningKey {
    SigningKey::from_seed(&[0x5c; 32])
}

/// The key document of `SLICE_HOST` for `slice_key`, valid from 2000-01-01.
pub fn slice_keydoc() -> Vec<u8> {
    let k = slice_key();
    let public: String = k.public_key().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        r#"{{"host":"{SLICE_HOST}","keys":[{{"alg":"ed25519","id":"{}","public":"{public}","valid_from":"2000-01-01T00:00:00.000000Z"}}],"proto":"datumesh/1"}}"#,
        k.key_id()
    )
    .into_bytes()
}

/// A store with the vectors' key documents and that of `SLICE_HOST`.
pub fn slice_store(db: &TestDb) -> TStore {
    let s = db.store();
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    s
}

/// `slice_store` on a PostgreSQL database alone (the Postgres-only tests).
pub fn slice_store_pg(db: &PgDb) -> PgStore {
    let s = db.store();
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    s
}

/// A `stored` descriptor of `SLICE_HOST` with the given arguments (name, type), returning a
/// dimensionless number; `body` tells functions with the same arguments apart.
pub fn stored_descriptor(args: &[(&str, Type)], body: &str) -> Vec<u8> {
    let decls: Vec<String> = args
        .iter()
        .map(|(n, t)| match t {
            Type::Number => format!(r#""{n}":{{"type":"number","unit":"1","valid":false}}"#),
            t => format!(r#""{n}":{{"type":"{}","valid":false}}"#, t.as_str()),
        })
        .collect();
    let text = format!(
        r#"{{"code":{{"body":"{body}","lang":"stored"}},"inputs":[],"signature":{{"args":{{{}}},"returns":{{"type":"number","unit":"1"}}}}}}"#,
        decls.join(",")
    );
    jcs(text.as_bytes())
}

/// The function hash of descriptor bytes.
pub fn fn_hash_of(desc: &[u8]) -> String {
    datumesh_core::function_hash(desc).unwrap()
}

/// A cell of function `fn_name` of `SLICE_HOST` with hash `fn_hash` and the given arguments.
pub fn slice_cell(fn_name: &str, fn_hash: &str, args: &[(&str, Scalar)]) -> datumesh_core::Cell {
    let members: Vec<(String, Value)> = args.iter().map(|(n, v)| (n.to_string(), v.to_value())).collect();
    let v = Value::Object(vec![
        ("args".into(), Value::Object(members)),
        ("fn".into(), Value::String(fn_name.into())),
        ("fn_hash".into(), Value::String(fn_hash.into())),
        ("ns".into(), Value::String(SLICE_HOST.into())),
    ]);
    datumesh_core::Cell::from_json(&v.to_jcs()).unwrap()
}

/// A scalar from its type and lexical form.
pub fn sc(t: Type, lexical: &str) -> Scalar {
    Scalar::from_lexical(t, lexical).unwrap()
}

/// A signed stored receipt of `cell` (no valid-time arguments) with a number `value`,
/// recorded at `tx`, as of `asof`, issued at `asof`.
pub fn slice_receipt(cell: &datumesh_core::Cell, value: &str, tx: &str, asof: &str) -> Vec<u8> {
    let r = ReceiptBuilder::new()
        .cell(cell.clone())
        .value(sc(Type::Number, value))
        .valid(Default::default())
        .asof(asof)
        .tx(tx)
        .issued(asof)
        .publisher(SLICE_HOST)
        .signer_host(SLICE_HOST)
        .licence("CC0-1.0")
        .summary("A slice test value.")
        .sign(&slice_key())
        .unwrap();
    receipt_json(&r)
}

/// Stores a receipt of `cell` (as `slice_receipt`) and records it as the cell's version at
/// `tx`; returns its hash.
pub fn put_member(
    s: &(impl AsStore + ?Sized),
    cell: &datumesh_core::Cell,
    value: &str,
    tx: &str,
    asof: &str,
) -> String {
    let s = s.st();
    let h = s.put_receipt(&slice_receipt(cell, value, tx, asof)).unwrap();
    s.put_version(&cell.id(), tx, &h).unwrap();
    h
}

/// A slice object from its JSON text.
pub fn slice(text: &str) -> datumesh_core::Slice {
    datumesh_core::Slice::from_json(text.as_bytes()).unwrap()
}

/// Stores everything a receipt vector offers: key documents, the recompute descriptor, the
/// input receipts (slice members included), the manifests and the receipt itself. Returns
/// the errors of the puts that failed (what the store refused), the receipt's put last.
pub fn put_vector_world(s: &(impl AsStore + ?Sized), v: &Value) -> Vec<(String, datumesh_store::Error)> {
    put_vector_keys(s, v);
    let s = s.st();
    let mut failed = Vec::new();
    if let Some(rc) = v.get("recompute") {
        if let Some(d) = rc.get("descriptor") {
            if let Err(e) = s.put_descriptor(&d.to_jcs()) {
                failed.push(("descriptor".to_string(), e));
            }
        }
        for (h, r) in rc.get("inputs").and_then(Value::as_object).unwrap_or(&[]) {
            if let Err(e) = s.put_receipt(r.as_str().unwrap().as_bytes()) {
                failed.push((format!("input {h}"), e));
            }
        }
        for (entry, m) in rc.get("manifests").and_then(Value::as_object).unwrap_or(&[]) {
            if let Err(e) = s.put_manifest(m.as_str().unwrap().as_bytes()) {
                failed.push((format!("manifest {entry}"), e));
            }
        }
    }
    if let Err(e) = s.put_receipt(&input(v)) {
        failed.push(("receipt".to_string(), e));
    }
    failed
}
