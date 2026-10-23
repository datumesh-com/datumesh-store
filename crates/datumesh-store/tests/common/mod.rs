//! Helpers shared by the integration tests (included with `mod common;`).
//!
//! Every test gets its own database, created from `DATABASE_URL` (a role allowed to create
//! databases) and dropped afterwards, so tests run in parallel without sharing state.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use datumesh_core::json::{self, Value};
use datumesh_core::{Keys, Receipt, ReceiptBuilder, Scalar, SigningKey, Type};
use datumesh_store::pg_unstable::postgres::{Client, Config, NoTls};
use datumesh_store::{migrate, pg_unstable, PgStore, PoolOptions, Store};

pub fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL names a PostgreSQL 16 database (see README)")
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

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

/// A database of its own, dropped when the value is dropped.
pub struct TestDb {
    pub name: String,
    /// The database's configuration, for plain-text clients (`NoTls`; `sslmode=prefer`
    /// then means no TLS).
    pub config: Config,
    admin: String,
}

impl TestDb {
    /// An empty database (no migrations applied).
    pub fn empty() -> TestDb {
        TestDb::empty_on(&admin_url())
    }

    /// An empty database on the server of `admin` (a URL whose role may create databases).
    pub fn empty_on(admin: &str) -> TestDb {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let name = format!("dm_test_{}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst), nanos);
        let admin_config: Config = admin.parse().unwrap();
        let mut c = admin_config.connect(NoTls).unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let mut config = admin_config.clone();
        config.dbname(&name);
        TestDb { name, config, admin: admin.to_string() }
    }

    /// A database with every migration applied (through a store, as `migrate::apply` takes).
    pub fn migrated() -> TestDb {
        let db = TestDb::empty();
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

impl Drop for TestDb {
    fn drop(&mut self) {
        let admin: Config = self.admin.parse().unwrap();
        if let Ok(mut c) = admin.connect(NoTls) {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name));
        }
    }
}

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

/// The number of rows of a table.
pub fn count(db: &TestDb, table: &str) -> i64 {
    db.client().query_one(&format!("SELECT count(*) FROM {table}"), &[]).unwrap().get(0)
}

/// The descriptor of the stored-number vector (fx_rate, stored), canonical.
pub fn fx_rate_descriptor() -> Vec<u8> {
    vector("receipt/stored-number").get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs()
}

/// When the key documents used by the tests were validated.
pub const VALIDATED_AT: &str = "2024-01-01T00:00:00.000000Z";

/// Stores every key document a vector offers (a 200 response with a body) as its host's
/// namespace.
pub fn put_vector_keys(s: &PgStore, v: &Value) {
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
