//! `PgStore`: the store on PostgreSQL.

use std::borrow::Cow;

use crate::error::{Db, Error, Result};
use crate::pg_unstable::Connector;
use crate::pool::{Conn, Pool, PoolOptions, Pooled};
use crate::sql::Q;
use crate::{Binding, Namespace, SliceMember, SliceValue, Store, Version};
use datumesh_core::json::Value;
use datumesh_core::lexical::{check_dns_name, check_function_name, check_hex, check_input_entry, check_instant};
use datumesh_core::{
    json, keys_from_response, sha256_hex, verify, Cell, ErrorCode, FunctionDescriptor, Keys, Manifest, Receipt,
    Resolver, Scalar, Slice, Type,
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

/// A slice entry, `slice:<slice id>:<members hash>` (`E_CANON_HEX` otherwise, as SPEC 10.1
/// step 3 says of a malformed `inputs` entry).
fn check_entry(entry: &str, path: &str) -> Result<()> {
    check_input_entry(entry, path)?;
    if !entry.starts_with("slice:") {
        return Err(Error::Invalid(datumesh_core::Error::new(ErrorCode::CanonHex, path, "not a slice entry")));
    }
    Ok(())
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

    /// Versions from rows of (tx, receipt_hash, asof).
    fn version_rows(rows: &[postgres::Row], cell_id: &str) -> Result<Vec<Version>> {
        rows.iter().map(|r| version_row(r, cell_id)).collect()
    }

    /// A version (and its receipt's bytes) from a query answering
    /// (tx, receipt_hash, asof, bytes) for (cell_id, asof).
    fn asof_with_receipt(
        &self,
        op: &'static str,
        q: Q,
        cell_id: &str,
        asof: &str,
    ) -> Result<Option<(Version, Vec<u8>)>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(asof, "asof")?;
        self.run(op, |c| {
            let st = c.stmt(q)?;
            let Some(row) = c.client.query_opt(&st, &[&cell_id, &asof]).db()? else { return Ok(None) };
            Ok(Some((version_row(&row, cell_id)?, row.try_get(3).db()?)))
        })
    }

    /// A version from a query answering (tx, receipt_hash, asof) for (cell_id, asof).
    fn asof_version(&self, op: &'static str, q: Q, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(asof, "asof")?;
        self.run(op, |c| {
            let st = c.stmt(q)?;
            let rows = c.client.query(&st, &[&cell_id, &asof]).db()?;
            Ok(PgStore::version_rows(&rows, cell_id)?.pop())
        })
    }
}

fn version_row(r: &postgres::Row, cell_id: &str) -> Result<Version> {
    Ok(Version {
        cell_id: cell_id.to_string(),
        tx: r.try_get(0).db()?,
        receipt_hash: r.try_get(1).db()?,
        asof: r.try_get(2).db()?,
    })
}

/// A member from a row of (tx, receipt_hash, asof, bytes, ...).
fn member_row(r: &postgres::Row, cell_id: &str) -> Result<SliceMember> {
    Ok(SliceMember { version: version_row(r, cell_id)?, receipt: r.try_get(3).db()? })
}

/// A member's value and cell from a row of (tx, receipt_hash, asof, value_type, value, ns,
/// fn, fn_hash, "name:type:value" of each argument, ...): the cell is rebuilt from the
/// typed-argument index (`cell_args` holds every argument's type and lexical form).
fn value_row(r: &postgres::Row, cell_id: &str) -> Result<SliceValue> {
    let bad =
        |what: &str| Error::Invalid(datumesh_core::Error::new(ErrorCode::CanonType, "$", format!("a stored {what}")));
    let scalar = |ty: &str, value: &str| -> Result<Scalar> {
        let ty = Type::from_name(ty).ok_or_else(|| bad("type"))?;
        Ok(Scalar::from_lexical(ty, value)?)
    };
    let (ty, value): (String, String) = (r.try_get(3).db()?, r.try_get(4).db()?);
    let mut args = datumesh_core::Args::new();
    for a in r.try_get::<_, Vec<String>>(8).db()? {
        let mut parts = a.splitn(3, ':');
        let (Some(name), Some(t), Some(v)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad("argument"));
        };
        args.insert(name.to_string(), datumesh_core::Arg::Value(scalar(t, v)?));
    }
    let cell = Cell { args, fn_hash: r.try_get(7).db()?, fn_name: r.try_get(6).db()?, ns: r.try_get(5).db()? };
    Ok(SliceValue { version: version_row(r, cell_id)?, value: scalar(&ty, &value)?, cell })
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

    /// The stored manifest of slice entry `entry` ([`Store::put_manifest`]), its canonical
    /// form; `None` when there is none, or `entry` is not an entry (since 0.3.1).
    fn manifest(&self, entry: &str) -> Option<Cow<'_, [u8]>> {
        if check_entry(entry, "$").is_err() {
            return None;
        }
        self.resolve("manifest", |c| get_bytes(c, Q::ManifestGet, entry)).map(Cow::Owned)
    }

    /// `Some` only for a `stored` function (descriptor `code.lang` is `stored`, and the store
    /// has the descriptor): the value of the receipt of the version that answers as of
    /// `asof`, [`stored_asof`](Store::stored_asof)'s: the latest version (by tx) with `tx <=
    /// asof` whose receipt's asof is also `<= asof` (the core's testkit
    /// `AnswerRule::TxAndAsof`). `None` for every other function, and when no version
    /// answers. (Until 0.2.0 it looked at `tx` only, as [`version_asof`](Store::version_asof)
    /// still does; the same when every receipt's asof is its tx.)
    fn evaluate(&self, cell: &Cell, asof: &str) -> Option<Scalar> {
        if !is_instant(asof) {
            return None;
        }
        self.resolve("evaluate", |c| {
            let Some(desc) = get_bytes(c, Q::DescriptorGet, &cell.fn_hash)? else { return Ok(None) };
            match FunctionDescriptor::from_json(&desc) {
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
        // spec-0.4.0's descriptor: `signature.order` and argument `extent` allowed.
        let d = FunctionDescriptor::from_json(bytes)?;
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
        // Receipt hashes are edges; slice entries (spec-0.4.0) slice edges.
        let (entries, inputs): (Vec<String>, Vec<String>) =
            r.inputs().iter().cloned().partition(|e| e.starts_with("slice:"));
        self.run("put_receipt", |c| {
            let ns = c.stmt(Q::NamespaceGet)?;
            let put = c.stmt(Q::ReceiptPut)?;
            let get = c.stmt(Q::ReceiptGet)?;
            let edges = c.stmt(Q::EdgesPut)?;
            let slice_edges = c.stmt(Q::SliceEdgesPut)?;
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
            let n = t
                .execute(&put, &[&hash, &canonical, &cell_id, &r.tx(), &r.asof(), &r.cell().fn_hash, &r.publisher()])
                .db()?;
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
            if !entries.is_empty() {
                t.execute(&slice_edges, &[&hash, &entries]).db()?;
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
            let latest = c.stmt(Q::VersionLatestTx)?;
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
        self.asof_version("version_asof", Q::VersionAsof, cell_id, asof)
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

    fn put_computed(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(tx, "tx")?;
        check_hex(receipt_hash, 64, "receipt_hash")?;
        // No lock: the primary key serialises writers of one (cell_id, tx, asof). An insert
        // that meets a concurrent one waits for it; if it committed, nothing is inserted and
        // the row it wrote (visible to the next statement under READ COMMITTED) decides.
        let asof = self.run("put_computed", |c| {
            let meta = c.stmt(Q::ReceiptMeta)?;
            let put = c.stmt(Q::ComputedPut)?;
            let get = c.stmt(Q::ComputedGet)?;
            let mut t = c.client.transaction().db()?;
            let Some(row) = t.query_opt(&meta, &[&receipt_hash]).db()? else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            let (r_cell, r_tx, asof): (String, String, String) =
                (row.try_get(0).db()?, row.try_get(1).db()?, row.try_get(2).db()?);
            if r_cell != cell_id {
                return Err(Error::Mismatch { what: "cell_id", expected: cell_id.to_string(), found: r_cell });
            }
            if r_tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: r_tx });
            }
            if t.execute(&put, &[&cell_id, &tx, &asof, &receipt_hash]).db()? == 0 {
                let stored: String = t.query_one(&get, &[&cell_id, &tx, &asof]).db()?.try_get(0).db()?;
                if stored != receipt_hash {
                    return Err(Error::Conflict { hash: receipt_hash.to_string() });
                }
            }
            t.commit().db()?;
            Ok(asof)
        })?;
        tracing::debug!(target: "datumesh_store", cell_id, tx, asof, receipt_hash, "put_computed");
        Ok(())
    }

    fn computed_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        self.asof_version("computed_asof", Q::ComputedAsof, cell_id, asof)
    }

    fn stored_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>> {
        self.asof_version("stored_asof", Q::StoredAsof, cell_id, asof)
    }

    fn cell_dependents(&self, cell_id: &str) -> Result<Vec<String>> {
        check_hex(cell_id, 64, "cell_id")?;
        self.run("cell_dependents", |c| get_strings(c, Q::CellDependents, cell_id))
    }

    /// One query: the version and its receipt's bytes.
    fn computed_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        Ok(self
            .asof_with_receipt("computed_asof_with_receipt", Q::ComputedAsofReceipt, cell_id, asof)?
            .map(|(v, b)| (v, Cow::Owned(b))))
    }

    /// One query: the version and its receipt's bytes.
    fn stored_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        Ok(self
            .asof_with_receipt("stored_asof_with_receipt", Q::StoredAsofReceipt, cell_id, asof)?
            .map(|(v, b)| (v, Cow::Owned(b))))
    }

    fn put_manifest(&self, bytes: &[u8]) -> Result<String> {
        let m = Manifest::from_json(bytes)?;
        let entry = m.entry();
        let canonical = m.canonical();
        let members = m.members();
        let n = i32::try_from(members.len()).unwrap_or(i32::MAX);
        self.run("put_manifest", |c| {
            let put = c.stmt(Q::ManifestPut)?;
            let put_members = c.stmt(Q::ManifestMembersPut)?;
            let mut t = c.client.transaction().db()?;
            // The entry hashes the content: a stored entry has these very bytes. A concurrent
            // insert of it waits for the other; if that one committed, nothing is inserted
            // here, members included (it inserted them).
            let inserted = t
                .execute(&put, &[&entry, &m.slice().id(), &m.members_hash(), &m.slice().fn_hash(), &n, &canonical])
                .db()?;
            if inserted == 1 && !members.is_empty() {
                t.execute(&put_members, &[&entry, &members]).db()?;
            }
            t.commit().db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", entry = %entry, members = n, "put_manifest");
        Ok(entry)
    }

    fn member_manifests(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("member_manifests", |c| get_strings(c, Q::MemberManifests, receipt_hash))
    }

    fn slice_entries(&self, receipt_hash: &str) -> Result<Vec<String>> {
        check_hex(receipt_hash, 64, "receipt_hash")?;
        self.run("slice_entries", |c| get_strings(c, Q::SliceEntries, receipt_hash))
    }

    fn slice_dependents(&self, entry: &str) -> Result<Vec<String>> {
        check_entry(entry, "entry")?;
        self.run("slice_dependents", |c| get_strings(c, Q::SliceDependents, entry))
    }

    fn slice_members(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceMember>> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.versions_between(&cell_id, &from, &to, asof);
        }
        let pattern = Pattern::of(&v)?;
        self.run("slice_members", |c| pattern.members(c, asof))
    }

    fn versions_between(&self, cell_id: &str, from: &str, to: &str, asof: &str) -> Result<Vec<SliceMember>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(from, "from")?;
        check_instant(to, "to")?;
        check_instant(asof, "asof")?;
        self.run("versions_between", |c| {
            let st = c.stmt(Q::VersionsBetween)?;
            let rows = c.client.query(&st, &[&cell_id, &from, &to, &asof]).db()?;
            rows.iter().map(|r| member_row(r, cell_id)).collect()
        })
    }

    fn slice_values(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceValue>> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.run("slice_values", |c| {
                let st = c.stmt(Q::VersionsBetweenValues)?;
                let rows = c.client.query(&st, &[&cell_id, &from, &to, &asof]).db()?;
                rows.iter().map(|r| value_row(r, &cell_id)).collect()
            });
        }
        let pattern = Pattern::of(&v)?;
        self.run("slice_values", |c| pattern.values(c, asof))
    }

    fn slice_changed_after(&self, slice: &Slice, asof: &str) -> Result<bool> {
        check_instant(asof, "asof")?;
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.run("slice_changed_after", |c| {
                let st = c.stmt(Q::VersionsChangedAfter)?;
                c.client.query_one(&st, &[&cell_id, &from, &to, &asof]).db()?.try_get(0).db()
            });
        }
        let pattern = Pattern::of(&v)?;
        self.run("slice_changed_after", |c| pattern.changed_after(c, asof))
    }

    fn resolve_binding(&self, fn_name: &str, asof: &str) -> Result<Option<Binding>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_binding", |c| {
            let st = c.stmt(Q::BindingAsof)?;
            let Some(row) = c.client.query_opt(&st, &[&fn_name, &asof]).db()? else { return Ok(None) };
            Ok(Some(Binding { fn_hash: row.try_get(0).db()?, tx: row.try_get(1).db()? }))
        })
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

/// A version slice's cell id, `from` and `to`; `None` for a slice of cells.
fn version_slice(slice: &Slice, v: &Value) -> Result<Option<(String, String, String)>> {
    if !slice.is_version_slice() {
        return Ok(None);
    }
    match (v.get("cell"), v.get("from").and_then(Value::as_str), v.get("to").and_then(Value::as_str)) {
        (Some(cell), Some(from), Some(to)) => Ok(Some((sha256_hex(&cell.to_jcs()), from.to_string(), to.to_string()))),
        _ => Err(Error::Invalid(datumesh_core::Error::new(ErrorCode::Schema, "$", "not a version slice"))),
    }
}

/// How a slice of cells constrains one argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Open,
    Value,
    Range,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Open => "open",
            Kind::Value => "value",
            Kind::Range => "range",
        }
    }
}

/// One argument of a slice of cells: its name, how it is constrained, and for a bound one
/// its type and lexical form (`lo` = `hi`), for a ranged one the range's type and ends.
struct Constraint {
    name: String,
    kind: Kind,
    ty: String,
    lo: String,
    hi: String,
}

/// A slice of cells (SPEC 4.4) as the member query takes it.
struct Pattern {
    fn_hash: String,
    ns: String,
    fn_name: String,
    args: Vec<Constraint>,
}

fn malformed(what: &str) -> Error {
    Error::Invalid(datumesh_core::Error::new(ErrorCode::Schema, "$", format!("not a slice object: {what}")))
}

impl Pattern {
    /// From the JSON form of a checked slice object of cells (`Slice::to_value`).
    fn of(v: &Value) -> Result<Pattern> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| malformed(k));
        let mut args = Vec::new();
        for (name, a) in v.get("args").and_then(Value::as_object).ok_or_else(|| malformed("args"))? {
            let text = |k: &str| match a.get(k) {
                Some(Value::String(x)) => Ok(x.clone()),
                Some(Value::Bool(b)) => Ok(b.to_string()),
                _ => Err(malformed(k)),
            };
            let c = match a {
                Value::String(x) if x == "*" => Constraint {
                    name: name.clone(),
                    kind: Kind::Open,
                    ty: String::new(),
                    lo: String::new(),
                    hi: String::new(),
                },
                Value::Object(m) if m.iter().any(|(k, _)| k == "lo") => Constraint {
                    name: name.clone(),
                    kind: Kind::Range,
                    ty: text("type")?,
                    lo: text("lo")?,
                    hi: text("hi")?,
                },
                _ => {
                    let value = text("value")?;
                    Constraint {
                        name: name.clone(),
                        kind: Kind::Value,
                        ty: text("type")?,
                        lo: value.clone(),
                        hi: value,
                    }
                }
            };
            args.push(c);
        }
        args.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Pattern { fn_hash: s("fn_hash")?, ns: s("ns")?, fn_name: s("fn")?, args })
    }

    /// The members as of `asof`, in key order.
    fn members(&self, c: &mut Conn, asof: &str) -> Result<Vec<SliceMember>> {
        let qs = [Q::SliceMembersAll, Q::SliceMembersText, Q::SliceMembersNum];
        self.rows(c, asof, qs, 4, member_row, |m| &m.version.cell_id)
    }

    /// The members' values as of `asof`, in key order.
    fn values(&self, c: &mut Conn, asof: &str) -> Result<Vec<SliceValue>> {
        let qs = [Q::SliceValuesAll, Q::SliceValuesText, Q::SliceValuesNum];
        self.rows(c, asof, qs, 9, value_row, |m| &m.version.cell_id)
    }

    /// The constraint columns of the member and change queries ($6-$10).
    fn columns(&self) -> [Vec<String>; 5] {
        let col = |f: fn(&Constraint) -> &str| -> Vec<String> { self.args.iter().map(|a| f(a).to_string()).collect() };
        [
            col(|a| a.name.as_str()),
            col(|a| a.kind.as_str()),
            col(|a| a.ty.as_str()),
            col(|a| a.lo.as_str()),
            col(|a| a.hi.as_str()),
        ]
    }

    /// Whether a matching cell has a stored version answering from after `asof`.
    fn changed_after(&self, c: &mut Conn, asof: &str) -> Result<bool> {
        let [names, kinds, types, los, his] = self.columns();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        let st = c.stmt(Q::SliceChangedAfter)?;
        c.client
            .query_one(
                &st,
                &[&self.fn_hash, &self.ns, &self.fn_name, &nargs, &asof, &names, &kinds, &types, &los, &his],
            )
            .db()?
            .try_get(0)
            .db()
    }

    /// The rows of one of the member queries `qs` (all cells, a text range, a number range),
    /// each made into a `T` (the cell id in column `at`, the key after it), in key order.
    fn rows<T>(
        &self,
        c: &mut Conn,
        asof: &str,
        qs: [Q; 3],
        at: usize,
        make: impl Fn(&postgres::Row, &str) -> Result<T>,
        cell_of: impl Fn(&T) -> &String,
    ) -> Result<Vec<T>> {
        let [names, kinds, types, los, his] = self.columns();
        let keys: Vec<String> = self.args.iter().filter(|a| a.kind != Kind::Value).map(|a| a.name.clone()).collect();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        // The candidates: the bound or ranged argument holding the fewest cells (probed when
        // there are several), else every cell of the function.
        let constrained: Vec<&Constraint> = self.args.iter().filter(|a| a.kind != Kind::Open).collect();
        let drive = match constrained.as_slice() {
            [] => None,
            [one] => Some(*one),
            several => {
                let mut best: Option<(i64, &Constraint)> = None;
                for a in several {
                    let q = if a.ty == "number" { Q::SliceProbeNum } else { Q::SliceProbeText };
                    let st = c.stmt(q)?;
                    let n: i64 = c
                        .client
                        .query_one(&st, &[&self.fn_hash, &a.name, &a.ty, &a.lo, &a.hi])
                        .db()?
                        .try_get(0)
                        .db()?;
                    if best.is_none_or(|(m, _)| n < m) {
                        best = Some((n, a));
                    }
                }
                best.map(|(_, a)| a)
            }
        };
        let (q, d_name, d_ty, d_lo, d_hi) = match drive {
            None => (qs[0], "", "", "", ""),
            Some(a) => {
                let q = if a.ty == "number" { qs[2] } else { qs[1] };
                (q, a.name.as_str(), a.ty.as_str(), a.lo.as_str(), a.hi.as_str())
            }
        };
        let st = c.stmt(q)?;
        let rows = c
            .client
            .query(
                &st,
                &[
                    &self.fn_hash,
                    &self.ns,
                    &self.fn_name,
                    &nargs,
                    &asof,
                    &names,
                    &kinds,
                    &types,
                    &los,
                    &his,
                    &keys,
                    &d_name,
                    &d_ty,
                    &d_lo,
                    &d_hi,
                ],
            )
            .db()?;
        let mut out: Vec<(Vec<String>, T)> = Vec::with_capacity(rows.len());
        for r in &rows {
            let cell_id: String = r.try_get(at).db()?;
            let key: Vec<String> = r.try_get(at + 1).db()?;
            out.push((key, make(r, &cell_id)?));
        }
        out.sort_by(|(a, ma), (b, mb)| cmp_keys(a, b).then_with(|| cell_of(ma).cmp(cell_of(mb))));
        Ok(out.into_iter().map(|(_, m)| m).collect())
    }
}

/// Key tuples of `type:value` items, compared item by item in each type's order (SPEC
/// 15.13): numbers numerically, everything else by its lexical form as bytes (dates and
/// instants are fixed width, so chronologically; strings by code point; `false` before
/// `true`; blobs as text). Items of different types (cells of one function hash whose
/// receipts disagree on an argument's type) by type name.
fn cmp_keys(a: &[String], b: &[String]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let (tx, vx) = x.split_once(':').unwrap_or(("", x));
        let (ty, vy) = y.split_once(':').unwrap_or(("", y));
        let o = tx.cmp(ty).then_with(|| if tx == "number" { cmp_decimal(vx, vy) } else { vx.cmp(vy) });
        if o.is_ne() {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Two numbers in canonical form (SPEC 3.1) compared as decimals.
pub(crate) fn cmp_decimal(a: &str, b: &str) -> std::cmp::Ordering {
    let (na, ma) = a.strip_prefix('-').map_or((false, a), |m| (true, m));
    let (nb, mb) = b.strip_prefix('-').map_or((false, b), |m| (true, m));
    let magnitude = || {
        let (ia, fa) = ma.split_once('.').unwrap_or((ma, ""));
        let (ib, fb) = mb.split_once('.').unwrap_or((mb, ""));
        // No leading zeros: the longer integer part is the greater; no trailing zeros: the
        // fractions compare as text.
        ia.len().cmp(&ib.len()).then_with(|| ia.cmp(ib)).then_with(|| fa.cmp(fb))
    };
    match (na, nb) {
        (false, true) => std::cmp::Ordering::Greater,
        (true, false) => std::cmp::Ordering::Less,
        (false, false) => magnitude(),
        (true, true) => magnitude().reverse(),
    }
}

#[cfg(test)]
mod tests {
    use super::cmp_decimal;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn decimals_compare_numerically() {
        for (a, b, o) in [
            ("9", "10", Less),
            ("-1.5", "-1", Less),
            ("-1", "-1.5", Greater),
            ("0", "-0.001", Greater),
            ("0.05", "0.5", Less),
            ("0.5", "0.51", Less),
            ("10.5", "9.75", Greater),
            ("-10", "-9", Less),
            ("123", "123", Equal),
            ("-0.1", "0", Less),
            ("1", "0.999999", Greater),
        ] {
            assert_eq!(cmp_decimal(a, b), o, "{a} {b}");
        }
    }
}
