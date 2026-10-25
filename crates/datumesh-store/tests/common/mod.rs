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
pub fn slice_store(db: &TestDb) -> PgStore {
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
pub fn put_member(s: &PgStore, cell: &datumesh_core::Cell, value: &str, tx: &str, asof: &str) -> String {
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
pub fn put_vector_world(s: &PgStore, v: &Value) -> Vec<(String, datumesh_store::Error)> {
    put_vector_keys(s, v);
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
