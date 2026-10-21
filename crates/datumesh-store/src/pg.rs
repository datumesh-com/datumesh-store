//! `PgStore`: the store on PostgreSQL.

use std::borrow::Cow;

use datumesh_core::lexical::{check_dns_name, check_function_name, check_hex, check_instant};
use datumesh_core::{json, keys_from_response, sha256_hex, Cell, Descriptor, Receipt, Resolver, Scalar};
use postgres::NoTls;

use crate::error::{Error, Result};
use crate::pool::{Conn, Pool, PoolOptions};
use crate::sql::Q;
use crate::{Namespace, Store, Version};

/// The store on PostgreSQL, with a connection pool. `Send + Sync`: share one per process
/// (behind an `Arc`); each call checks a connection out for its duration.
///
/// Every `put_*` is one transaction. Reads (the `Option` / `Vec` methods and [`Resolver`])
/// never fail loudly: a malformed argument is answered `None` / empty without a database
/// round trip, and a database error is logged with `tracing` (target `datumesh_store`,
/// level `error`) and answered `None` / empty.
pub struct PgStore {
    pool: Pool,
}

impl std::fmt::Debug for PgStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStore").finish_non_exhaustive()
    }
}

fn is_hash(s: &str) -> bool {
    check_hex(s, 64, "$").is_ok()
}

fn is_instant(s: &str) -> bool {
    check_instant(s, "$").is_ok()
}

impl PgStore {
    /// Connects to a `postgres://` URL or `key=value` connection string, without TLS, with
    /// the default [`PoolOptions`]. One connection is opened at once, so a wrong URL fails
    /// here. For TLS, use [`with_connector`](Self::with_connector).
    pub fn connect(url: &str) -> Result<PgStore> {
        let config: postgres::Config = url.parse()?;
        PgStore::with_connector(move || config.connect(NoTls), PoolOptions::new())
    }

    /// A store whose pool opens connections with `connect` (any TLS setup, any
    /// configuration). One connection is opened at once, so a failing connector fails here.
    pub fn with_connector<F>(connect: F, options: PoolOptions) -> Result<PgStore>
    where
        F: Fn() -> core::result::Result<postgres::Client, postgres::Error> + Send + Sync + 'static,
    {
        let store = PgStore { pool: Pool::new(Box::new(connect), options) };
        drop(store.pool.get()?);
        Ok(store)
    }

    /// Prepares every SQL statement the store uses against the live schema, on a fresh
    /// connection's behalf (no cached statement is reused), and returns how many there are.
    /// Fails with the first statement the schema cannot serve.
    pub fn check_statements(&self) -> Result<usize> {
        let mut c = self.pool.get()?;
        for q in Q::ALL {
            c.client.prepare_typed(q.sql(), q.types())?;
        }
        Ok(Q::ALL.len())
    }

    /// Runs a read; a database error is logged and becomes `None`.
    fn read<T>(&self, op: &'static str, f: impl FnOnce(&mut Conn) -> Result<T>) -> Option<T> {
        let result = self.pool.get().and_then(|mut c| f(&mut c));
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::error!(target: "datumesh_store", op, error = %e, "read failed; answering None");
                None
            }
        }
    }

    fn write<T>(&self, op: &'static str, f: impl FnOnce(&mut Conn) -> Result<T>) -> Result<T> {
        let result = self.pool.get().and_then(|mut c| f(&mut c));
        if let Err(e @ (Error::Database(_) | Error::PoolTimeout)) = &result {
            tracing::error!(target: "datumesh_store", op, error = %e, "write failed");
        }
        result
    }

    fn version_rows(rows: &[postgres::Row], cell_id: &str) -> Result<Vec<Version>> {
        rows.iter()
            .map(|r| Ok(Version { cell_id: cell_id.to_string(), tx: r.try_get(0)?, receipt_hash: r.try_get(1)? }))
            .collect()
    }
}

fn get_bytes(c: &mut Conn, q: Q, key: &str) -> Result<Option<Vec<u8>>> {
    let st = c.stmt(q)?;
    let row = c.client.query_opt(&st, &[&key])?;
    Ok(match row {
        Some(r) => Some(r.try_get(0)?),
        None => None,
    })
}

fn get_strings(c: &mut Conn, q: Q, key: &str) -> Result<Vec<String>> {
    let st = c.stmt(q)?;
    let rows = c.client.query(&st, &[&key])?;
    rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
}

impl Resolver for PgStore {
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(fn_hash) {
            return None;
        }
        self.read("descriptor", |c| get_bytes(c, Q::DescriptorGet, fn_hash)).flatten().map(Cow::Owned)
    }

    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(hash) {
            return None;
        }
        self.read("receipt", |c| get_bytes(c, Q::ReceiptGet, hash)).flatten().map(Cow::Owned)
    }

    /// `Some` only for a `stored` function (descriptor `code.lang` is `stored`, and the store
    /// has the descriptor): the value of the receipt of the cell's latest version with
    /// `tx <= asof`. `None` for every other function, and when no version is that old.
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<Scalar> {
        if !is_instant(asof) {
            return None;
        }
        self.read("evaluate", |c| {
            let Some(desc) = get_bytes(c, Q::DescriptorGet, &cell.fn_hash)? else { return Ok(None) };
            match Descriptor::from_json(&desc) {
                Ok(d) if d.lang == "stored" => {}
                Ok(_) => return Ok(None),
                Err(e) => {
                    tracing::error!(target: "datumesh_store", fn_hash = %cell.fn_hash, error = %e, "stored descriptor does not parse");
                    return Ok(None);
                }
            }
            let st = c.stmt(Q::ValueAsof)?;
            let Some(row) = c.client.query_opt(&st, &[&cell.id(), &asof])? else { return Ok(None) };
            let bytes: Vec<u8> = row.try_get(0)?;
            match Receipt::from_json(&bytes) {
                Ok(r) => Ok(Some(r.value().clone())),
                Err(e) => {
                    tracing::error!(target: "datumesh_store", cell_id = %cell.id(), error = %e, "stored receipt does not parse");
                    Ok(None)
                }
            }
        })
        .flatten()
    }
}

impl Store for PgStore {
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String> {
        let d = Descriptor::from_json(bytes)?;
        let hash = d.hash();
        let canonical = d.canonical();
        self.write("put_descriptor", |c| {
            let st = c.stmt(Q::DescriptorPut)?;
            c.client.execute(&st, &[&hash, &canonical])?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_hash = %hash, "put_descriptor");
        Ok(hash)
    }

    fn put_receipt(&self, bytes: &[u8]) -> Result<String> {
        let r = Receipt::from_json(bytes)?;
        // Steps 1-4 passed, so the text parses; its JCS is the canonical form.
        let canonical = json::parse(bytes)?.to_jcs();
        let hash = r.hash().to_string();
        let cell_id = r.cell().id();
        let inputs: Vec<String> = r.inputs().to_vec();
        self.write("put_receipt", |c| {
            let put = c.stmt(Q::ReceiptPut)?;
            let get = c.stmt(Q::ReceiptGet)?;
            let edges = c.stmt(Q::EdgesPut)?;
            let mut t = c.client.transaction()?;
            let n = t.execute(&put, &[&hash, &canonical, &cell_id, &r.tx(), &r.cell().fn_hash, &r.publisher()])?;
            if n == 0 {
                let row = t.query_one(&get, &[&hash])?;
                let stored: Vec<u8> = row.try_get(0)?;
                if stored != canonical {
                    return Err(Error::Conflict { hash: hash.clone() });
                }
            }
            if !inputs.is_empty() {
                t.execute(&edges, &[&hash, &inputs])?;
            }
            t.commit()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", hash = %hash, cell_id = %cell_id, "put_receipt");
        Ok(hash)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let length = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        self.write("put_blob", |c| {
            let st = c.stmt(Q::BlobPut)?;
            c.client.execute(&st, &[&sha, &bytes, &length])?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", sha256 = %sha, length, "put_blob");
        Ok(sha)
    }

    fn blob(&self, sha256: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(sha256) {
            return None;
        }
        self.read("blob", |c| get_bytes(c, Q::BlobGet, sha256)).flatten().map(Cow::Owned)
    }

    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(tx, "tx")?;
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.write("put_version", |c| {
            let lock = c.stmt(Q::Lock)?;
            let meta = c.stmt(Q::ReceiptMeta)?;
            let latest = c.stmt(Q::VersionLatest)?;
            let put = c.stmt(Q::VersionPut)?;
            let mut t = c.client.transaction()?;
            t.execute(&lock, &[&"versions", &cell_id])?;
            let Some(row) = t.query_opt(&meta, &[&receipt_hash])? else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            let (r_cell, r_tx): (String, String) = (row.try_get(0)?, row.try_get(1)?);
            if r_cell != cell_id {
                return Err(Error::Mismatch { what: "cell_id", expected: cell_id.to_string(), found: r_cell });
            }
            if r_tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: r_tx });
            }
            if let Some(row) = t.query_opt(&latest, &[&cell_id])? {
                let l: String = row.try_get(0)?;
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "version",
                        key: cell_id.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            t.execute(&put, &[&cell_id, &tx, &receipt_hash])?;
            t.commit()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", cell_id, tx, receipt_hash, "put_version");
        Ok(())
    }

    fn version_asof(&self, cell_id: &str, asof: &str) -> Option<Version> {
        if !is_hash(cell_id) || !is_instant(asof) {
            return None;
        }
        self.read("version_asof", |c| {
            let st = c.stmt(Q::VersionAsof)?;
            let rows = c.client.query(&st, &[&cell_id, &asof])?;
            Ok(PgStore::version_rows(&rows, cell_id)?.pop())
        })
        .flatten()
    }

    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Vec<Version> {
        if !is_hash(cell_id) || !is_instant(from) || !is_instant(to) {
            return Vec::new();
        }
        self.read("versions", |c| {
            let st = c.stmt(Q::VersionRange)?;
            let rows = c.client.query(&st, &[&cell_id, &from, &to])?;
            PgStore::version_rows(&rows, cell_id)
        })
        .unwrap_or_default()
    }

    fn latest(&self, cell_id: &str) -> Option<Version> {
        if !is_hash(cell_id) {
            return None;
        }
        self.read("latest", |c| {
            let st = c.stmt(Q::VersionLatest)?;
            let rows = c.client.query(&st, &[&cell_id])?;
            Ok(PgStore::version_rows(&rows, cell_id)?.pop())
        })
        .flatten()
    }

    fn dependents(&self, receipt_hash: &str) -> Vec<String> {
        if !is_hash(receipt_hash) {
            return Vec::new();
        }
        self.read("dependents", |c| get_strings(c, Q::Dependents, receipt_hash)).unwrap_or_default()
    }

    fn inputs(&self, receipt_hash: &str) -> Vec<String> {
        if !is_hash(receipt_hash) {
            return Vec::new();
        }
        self.read("inputs", |c| get_strings(c, Q::Inputs, receipt_hash)).unwrap_or_default()
    }

    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()> {
        // SPEC 9.3: a 200 with this body, for this host (a DNS name equal to the document's).
        keys_from_response(host, 200, Some(keydoc_bytes))?;
        check_instant(validated_at, "validated_at")?;
        let canonical = json::parse(keydoc_bytes)?.to_jcs();
        self.write("put_namespace", |c| {
            let lock = c.stmt(Q::Lock)?;
            let get = c.stmt(Q::NamespaceGet)?;
            let put = c.stmt(Q::NamespacePut)?;
            let mut t = c.client.transaction()?;
            t.execute(&lock, &[&"namespaces", &host])?;
            if let Some(row) = t.query_opt(&get, &[&host])? {
                let at: String = row.try_get(1)?;
                if validated_at < at.as_str() {
                    return Err(Error::NotLater {
                        what: "namespace",
                        key: host.to_string(),
                        given: validated_at.to_string(),
                        latest: at,
                    });
                }
            }
            t.execute(&put, &[&host, &canonical, &validated_at])?;
            t.commit()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", host, validated_at, "put_namespace");
        Ok(())
    }

    fn namespace(&self, host: &str) -> Option<Namespace> {
        if check_dns_name(host, "$").is_err() {
            return None;
        }
        self.read("namespace", |c| {
            let st = c.stmt(Q::NamespaceGet)?;
            let Some(row) = c.client.query_opt(&st, &[&host])? else { return Ok(None) };
            Ok(Some(Namespace {
                host: host.to_string(),
                keydoc: row.try_get(0)?,
                validated_at: row.try_get(1)?,
                suspended_at: row.try_get(2)?,
            }))
        })
        .flatten()
    }

    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()> {
        check_function_name(fn_name, "fn_name")?;
        check_hex(fn_hash, 64, "fn_hash")?;
        check_instant(tx, "tx")?;
        self.write("bind_name", |c| {
            let lock = c.stmt(Q::Lock)?;
            let exists = c.stmt(Q::DescriptorExists)?;
            let latest = c.stmt(Q::NameLatest)?;
            let put = c.stmt(Q::NamePut)?;
            let mut t = c.client.transaction()?;
            t.execute(&lock, &[&"names", &fn_name])?;
            let found: bool = t.query_one(&exists, &[&fn_hash])?.try_get(0)?;
            if !found {
                return Err(Error::NotFound { what: "descriptor", key: fn_hash.to_string() });
            }
            if let Some(row) = t.query_opt(&latest, &[&fn_name])? {
                let l: String = row.try_get(0)?;
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "name",
                        key: fn_name.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            t.execute(&put, &[&fn_name, &tx, &fn_hash])?;
            t.commit()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_name, fn_hash, tx, "bind_name");
        Ok(())
    }

    fn resolve_name(&self, fn_name: &str, asof: &str) -> Option<String> {
        if check_function_name(fn_name, "$").is_err() || !is_instant(asof) {
            return None;
        }
        self.read("resolve_name", |c| {
            let st = c.stmt(Q::NameAsof)?;
            let row = c.client.query_opt(&st, &[&fn_name, &asof])?;
            Ok(match row {
                Some(r) => Some(r.try_get(0)?),
                None => None,
            })
        })
        .flatten()
    }
}
