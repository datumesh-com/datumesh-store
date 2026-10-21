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
use datumesh_store::postgres::{Client, Config, NoTls};
use datumesh_store::{migrate, PgStore, PoolOptions};

pub fn admin_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL names a PostgreSQL 16 database (see README)")
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A database of its own, dropped when the value is dropped.
pub struct TestDb {
    pub name: String,
    pub config: Config,
}

impl TestDb {
    /// An empty database (no migrations applied).
    pub fn empty() -> TestDb {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let name = format!("dm_test_{}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst), nanos);
        let admin: Config = admin_url().parse().unwrap();
        let mut c = admin.connect(NoTls).unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let mut config = admin.clone();
        config.dbname(&name);
        TestDb { name, config }
    }

    /// A database with every migration applied.
    pub fn migrated() -> TestDb {
        let db = TestDb::empty();
        migrate::apply(&mut db.client()).unwrap();
        db
    }

    pub fn client(&self) -> Client {
        self.config.connect(NoTls).unwrap()
    }

    pub fn store(&self) -> PgStore {
        self.store_with(PoolOptions::new())
    }

    pub fn store_with(&self, options: PoolOptions) -> PgStore {
        let config = self.config.clone();
        PgStore::with_connector(move || config.connect(NoTls), options).unwrap()
    }

    /// `postgres://…/<this database>` for `PgStore::connect`.
    pub fn url(&self) -> String {
        let admin = admin_url();
        let (base, _) = admin.rsplit_once('/').unwrap();
        format!("{base}/{}", self.name)
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let admin: Config = admin_url().parse().unwrap();
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
    let v = vector("receipt/stored-number");
    let base = Receipt::from_json(&input(&v)).unwrap();
    let r = ReceiptBuilder::new()
        .cell(base.cell().clone())
        .value(Scalar::from_lexical(Type::Number, value).unwrap())
        .valid(base.valid().clone())
        .asof(tx)
        .tx(tx)
        .issued(tx)
        .publisher(base.publisher())
        .signer_host(base.signer_host())
        .licence(base.licence())
        .summary(format!("1 EUR was worth {value} USD at the 15 March 2024 reference rate."))
        .sign(&signing_key(&v))
        .unwrap();
    receipt_json(&r)
}

/// The descriptor of the stored-number vector (fx_rate, stored), canonical.
pub fn fx_rate_descriptor() -> Vec<u8> {
    vector("receipt/stored-number").get("recompute").and_then(|r| r.get("descriptor")).unwrap().to_jcs()
}
