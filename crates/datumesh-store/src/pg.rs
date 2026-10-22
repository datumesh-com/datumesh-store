//! `PgStore`: the store on PostgreSQL.

use std::borrow::Cow;

use crate::error::{Db, Error, Result};
use crate::pg_unstable::Connector;
use crate::pool::{Conn, Pool, PoolOptions, Pooled};
use crate::sql::Q;
use crate::{Namespace, Store, Version};
use datumesh_core::lexical::{check_dns_name, check_function_name, check_hex, check_instant};
use datumesh_core::{
    json, keys_from_response, sha256_hex, verify, Cell, Descriptor, ErrorCode, Keys, Receipt, Resolver, Scalar,
};

/// The store on PostgreSQL, with a connection pool. `Send + Sync`: share one per process
/// (behind an `Arc`); each call checks a connection out for its duration.
///
/// Every `put_*` is one transaction. [`Store`] methods return errors: a malformed argument
/// is [`Error::Invalid`] (no query is made), a database failure [`Error::Database`]. The
/// [`Resolver`] methods answer both with `None`; a database failure is logged with
/// `tracing` (target `datumesh_store`, level `error`).
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
    /// Connects to a `postgres://` URL or `key=value` connection string with the default
    /// [`PoolOptions`]: [`connect_with(url, PoolOptions::new())`](Self::connect_with).
    pub fn connect(url: &str) -> Result<PgStore> {
        PgStore::connect_with(url, PoolOptions::new())
    }

    /// Connects to a `postgres://` URL or `key=value` connection string, with a pool and TLS
    /// roots as `options` say. One connection is opened at once, so a wrong URL, an
    /// unreachable server or a refused certificate fails here; nothing else is done on it
    /// (no statement is prepared, the schema is not looked at), so an empty database is fine
    /// (create the schema with [`migrate::apply`](crate::migrate::apply)).
    ///
    /// TLS (rustls) is used as the URL's `sslmode` says: `disable` never, `prefer` (the
    /// default) when the server offers it, `require` always. Whenever it is used, the
    /// server's certificate and host name are verified, against the Mozilla roots plus the
    /// roots added with [`PoolOptions::tls_root_certificates_pem`]; there is no way to
    /// accept any certificate. So a server with `ssl=on` and a self-signed certificate (or
    /// one from a private CA) fails even under the default `prefer`: add its CA, or connect
    /// with `sslmode=disable`.
    pub fn connect_with(url: &str, options: PoolOptions) -> Result<PgStore> {
        let config: postgres::Config = url.parse().db()?;
        let connector = Connector::new(config, &options)?;
        PgStore::from_pool(Pool::new(Box::new(move || connector.connect()), options))
    }

    /// A store on `pool`, after opening one connection.
    pub(crate) fn from_pool(pool: Pool) -> Result<PgStore> {
        let store = PgStore { pool };
        drop(store.pool.get()?);
        Ok(store)
    }

    /// A pooled connection, held until dropped (migrations hold one throughout).
    pub(crate) fn conn(&self) -> Result<Pooled<'_>> {
        self.pool.get()
    }

    /// Prepares every SQL statement the store uses against the live schema, on a fresh
    /// connection's behalf (no cached statement is reused), and returns how many there are.
    /// Fails with the first statement the schema cannot serve.
    pub fn check_statements(&self) -> Result<usize> {
        let mut c = self.pool.get()?;
        for q in Q::ALL {
            c.client.prepare_typed(q.sql(), q.types()).db()?;
        }
        Ok(Q::ALL.len())
    }

    /// A [`Resolver`] lookup: a failure (logged by `run`) becomes `None`.
    fn resolve<T>(&self, op: &'static str, f: impl FnOnce(&mut Conn) -> Result<Option<T>>) -> Option<T> {
        self.run(op, f).ok().flatten()
    }

    /// Runs `f` on a pooled connection; a database or pool failure is logged.
    fn run<T>(&self, op: &'static str, f: impl FnOnce(&mut Conn) -> Result<T>) -> Result<T> {
        let result = self.pool.get().and_then(|mut c| f(&mut c));
        if let Err(e @ (Error::Database(_) | Error::PoolTimeout)) = &result {
            tracing::error!(target: "datumesh_store", op, error = %e, "database failure");
        }
        result
    }

    fn version_rows(rows: &[postgres::Row], cell_id: &str) -> Result<Vec<Version>> {
        rows.iter()
            .map(|r| {
                Ok(Version { cell_id: cell_id.to_string(), tx: r.try_get(0).db()?, receipt_hash: r.try_get(1).db()? })
            })
            .collect()
    }
}

fn get_bytes(c: &mut Conn, q: Q, key: &str) -> Result<Option<Vec<u8>>> {
    let st = c.stmt(q)?;
    let row = c.client.query_opt(&st, &[&key]).db()?;
    Ok(match row {
        Some(r) => Some(r.try_get(0).db()?),
        None => None,
    })
}

fn get_strings(c: &mut Conn, q: Q, key: &str) -> Result<Vec<String>> {
    let st = c.stmt(q)?;
    let rows = c.client.query(&st, &[&key]).db()?;
    rows.iter().map(|r| r.try_get(0).db()).collect()
}

impl Resolver for PgStore {
    fn descriptor(&self, fn_hash: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(fn_hash) {
            return None;
        }
        self.resolve("descriptor", |c| get_bytes(c, Q::DescriptorGet, fn_hash)).map(Cow::Owned)
    }

    fn receipt(&self, hash: &str) -> Option<Cow<'_, [u8]>> {
        if !is_hash(hash) {
            return None;
        }
        self.resolve("receipt", |c| get_bytes(c, Q::ReceiptGet, hash)).map(Cow::Owned)
    }

    /// `Some` only for a `stored` function (descriptor `code.lang` is `stored`, and the store
    /// has the descriptor): the value of the receipt of the cell's latest version with
    /// `tx <= asof`. `None` for every other function, and when no version is that old.
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<Scalar> {
        if !is_instant(asof) {
            return None;
        }
        self.resolve("evaluate", |c| {
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
            let Some(row) = c.client.query_opt(&st, &[&cell.id(), &asof]).db()? else { return Ok(None) };
            let bytes: Vec<u8> = row.try_get(0).db()?;
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

impl Store for PgStore {
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String> {
        let d = Descriptor::from_json(bytes)?;
        let hash = d.hash();
        let canonical = d.canonical();
        self.run("put_descriptor", |c| {
            let st = c.stmt(Q::DescriptorPut)?;
            c.client.execute(&st, &[&hash, &canonical]).db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_hash = %hash, "put_descriptor");
        Ok(hash)
    }

    fn put_receipt(&self, bytes: &[u8]) -> Result<String> {
        // Steps 1-4 first: a receipt that is not one is refused without a query.
        let r = Receipt::from_json(bytes)?;
        // Steps 1-4 passed, so the text parses; its JCS is the canonical form.
        let canonical = json::parse(bytes)?.to_jcs();
        let hash = r.hash().to_string();
        let cell_id = r.cell().id();
        let host = r.signer_host().to_string();
        let inputs: Vec<String> = r.inputs().to_vec();
        self.run("put_receipt", |c| {
            let ns = c.stmt(Q::NamespaceGet)?;
            let put = c.stmt(Q::ReceiptPut)?;
            let get = c.stmt(Q::ReceiptGet)?;
            let edges = c.stmt(Q::EdgesPut)?;
            let mut t = c.client.transaction().db()?;
            // Idempotent: the very receipt already stored verified when it was stored, and
            // the stored copy is unchanged, so it is not checked again (even if its key has
            // since been revoked).
            if let Some(row) = t.query_opt(&get, &[&hash]).db()? {
                let stored: Vec<u8> = row.try_get(0).db()?;
                if stored == canonical {
                    return Ok(());
                }
            }
            // Steps 5-6 against the stored key document of the signer host; none stored is
            // a host without keys (E_KEY_UNAVAILABLE).
            let mut keys = Keys::new();
            if let Some(row) = t.query_opt(&ns, &[&host]).db()? {
                let doc: Vec<u8> = row.try_get(0).db()?;
                keys.insert_response(&host, 200, Some(&doc));
            }
            let out = verify(bytes, &keys, None, None);
            if !out.ok {
                return Err(Error::Invalid(out.error().unwrap_or_else(|| {
                    datumesh_core::Error::new(ErrorCode::Signature, "$", "the receipt did not verify")
                })));
            }
            let n = t.execute(&put, &[&hash, &canonical, &cell_id, &r.tx(), &r.cell().fn_hash, &r.publisher()]).db()?;
            if n == 0 {
                let row = t.query_one(&get, &[&hash]).db()?;
                let stored: Vec<u8> = row.try_get(0).db()?;
                if stored != canonical {
                    return Err(Error::Conflict { hash: hash.clone() });
                }
            }
            if !inputs.is_empty() {
                t.execute(&edges, &[&hash, &inputs]).db()?;
            }
            t.commit().db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", hash = %hash, cell_id = %cell_id, "put_receipt");
        Ok(hash)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<String> {
        let sha = sha256_hex(bytes);
        let length = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        self.run("put_blob", |c| {
            let st = c.stmt(Q::BlobPut)?;
            c.client.execute(&st, &[&sha, &bytes, &length]).db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", sha256 = %sha, length, "put_blob");
        Ok(sha)
    }

    fn blob(&self, sha256: &str) -> Result<Option<Cow<'_, [u8]>>> {
        check_hex(sha256, 64, "sha256")?;
        Ok(self.run("blob", |c| get_bytes(c, Q::BlobGet, sha256))?.map(Cow::Owned))
    }

    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(tx, "tx")?;
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("put_version", |c| {
            let lock = c.stmt(Q::Lock)?;
            let meta = c.stmt(Q::ReceiptMeta)?;
            let latest = c.stmt(Q::VersionLatest)?;
            let put = c.stmt(Q::VersionPut)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"versions", &cell_id]).db()?;
            let Some(row) = t.query_opt(&meta, &[&receipt_hash]).db()? else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            let (r_cell, r_tx): (String, String) = (row.try_get(0).db()?, row.try_get(1).db()?);
            if r_cell != cell_id {
                return Err(Error::Mismatch { what: "cell_id", expected: cell_id.to_string(), found: r_cell });
            }
            if r_tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: r_tx });
            }
            if let Some(row) = t.query_opt(&latest, &[&cell_id]).db()? {
                let l: String = row.try_get(0).db()?;
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "version",
                        key: cell_id.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            t.execute(&put, &[&cell_id, &tx, &receipt_hash]).db()?;
            t.commit().db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", cell_id, tx, receipt_hash, "put_version");
        Ok(())
    }

    fn version_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(asof, "asof")?;
        self.run("version_asof", |c| {
            let st = c.stmt(Q::VersionAsof)?;
            let rows = c.client.query(&st, &[&cell_id, &asof]).db()?;
            Ok(PgStore::version_rows(&rows, cell_id)?.pop())
        })
    }

    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Result<Vec<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(from, "from")?;
        check_instant(to, "to")?;
        self.run("versions", |c| {
            let st = c.stmt(Q::VersionRange)?;
            let rows = c.client.query(&st, &[&cell_id, &from, &to]).db()?;
            PgStore::version_rows(&rows, cell_id)
        })
    }

    fn latest(&self, cell_id: &str) -> Result<Option<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        self.run("latest", |c| {
            let st = c.stmt(Q::VersionLatest)?;
            let rows = c.client.query(&st, &[&cell_id]).db()?;
            Ok(PgStore::version_rows(&rows, cell_id)?.pop())
        })
    }

    fn dependents(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("dependents", |c| get_strings(c, Q::Dependents, receipt_hash))
    }

    fn inputs(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("inputs", |c| get_strings(c, Q::Inputs, receipt_hash))
    }

    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()> {
        // SPEC 9.3: a 200 with this body, for this host (a DNS name equal to the document's).
        keys_from_response(host, 200, Some(keydoc_bytes))?;
        check_instant(validated_at, "validated_at")?;
        let canonical = json::parse(keydoc_bytes)?.to_jcs();
        self.run("put_namespace", |c| {
            let lock = c.stmt(Q::Lock)?;
            let get = c.stmt(Q::NamespaceGet)?;
            let put = c.stmt(Q::NamespacePut)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"namespaces", &host]).db()?;
            if let Some(row) = t.query_opt(&get, &[&host]).db()? {
                let at: String = row.try_get(1).db()?;
                if validated_at < at.as_str() {
                    return Err(Error::NotLater {
                        what: "namespace",
                        key: host.to_string(),
                        given: validated_at.to_string(),
                        latest: at,
                    });
                }
            }
            t.execute(&put, &[&host, &canonical, &validated_at]).db()?;
            t.commit().db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", host, validated_at, "put_namespace");
        Ok(())
    }

    fn namespace(&self, host: &str) -> Result<Option<Namespace>> {
        check_dns_name(host, "host")?;
        self.run("namespace", |c| {
            let st = c.stmt(Q::NamespaceGet)?;
            let Some(row) = c.client.query_opt(&st, &[&host]).db()? else { return Ok(None) };
            Ok(Some(Namespace {
                host: host.to_string(),
                keydoc: row.try_get(0).db()?,
                validated_at: row.try_get(1).db()?,
                suspended_at: row.try_get(2).db()?,
            }))
        })
    }

    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()> {
        check_function_name(fn_name, "fn_name")?;
        check_hex(fn_hash, 64, "fn_hash")?;
        check_instant(tx, "tx")?;
        self.run("bind_name", |c| {
            let lock = c.stmt(Q::Lock)?;
            let exists = c.stmt(Q::DescriptorExists)?;
            let latest = c.stmt(Q::NameLatest)?;
            let put = c.stmt(Q::NamePut)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"names", &fn_name]).db()?;
            let found: bool = t.query_one(&exists, &[&fn_hash]).db()?.try_get(0).db()?;
            if !found {
                return Err(Error::NotFound { what: "descriptor", key: fn_hash.to_string() });
            }
            if let Some(row) = t.query_opt(&latest, &[&fn_name]).db()? {
                let l: String = row.try_get(0).db()?;
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "name",
                        key: fn_name.to_string(),
                        given: tx.to_string(),
                        latest: l,
                    });
                }
            }
            t.execute(&put, &[&fn_name, &tx, &fn_hash]).db()?;
            t.commit().db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", fn_name, fn_hash, tx, "bind_name");
        Ok(())
    }

    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_name", |c| {
            let st = c.stmt(Q::NameAsof)?;
            let row = c.client.query_opt(&st, &[&fn_name, &asof]).db()?;
            Ok(match row {
                Some(r) => Some(r.try_get(0).db()?),
                None => None,
            })
        })
    }
}
