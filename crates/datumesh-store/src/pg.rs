//! `PgStore`: the store on PostgreSQL.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::catalogue::{
    args_json, as_i64, canonical_examples, check_catalog_entry, check_catalog_ext, check_key_values, check_model,
    entry_ext, entry_from_columns, entry_text, invalid, miss_text, parse_facets, rank, value_text, vector_literal,
    CatalogRow, Words, SUMMARY_VALUES,
};
use crate::error::{Db, Error, Result};
use crate::pg_unstable::Connector;
use crate::pool::{Conn, Pool, PoolOptions, Pooled};
use crate::slices::{check_entry, cmp_keys, is_hash, is_instant, version_slice, Constraint, Kind, Pattern};
use crate::sql::Q;
use crate::{
    later_than, not_canonical_class, parse_receipt, stored_value, unbound, Binding, CatalogEntry, CatalogEntryExt,
    CatalogKind, CellWrite, ClassBinding, Dictionary, DictionaryValue, EmbedRef, Embedding, FacetedEntry, FunctionTest,
    Hit, KeyValue, Namespace, Parsed, PendingEmbedding, SliceMember, SliceValue, Store, Summary, SummaryEntry,
    TestSummary, Version, FIND_MISSES_CAP,
};
use datumesh_core::lexical::{check_dns_name, check_function_name, check_hex, check_instant};
use datumesh_core::{
    json, keys_from_response, sha256_hex, verify, Cell, ClassDescriptor, ErrorCode, FunctionDescriptor, Keys, Manifest,
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
    /// Whether the `embeddings` table exists (the `vector` extension was available when the
    /// schema was migrated), looked at when the store was opened.
    vectors: bool,
}

impl std::fmt::Debug for PgStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgStore").finish_non_exhaustive()
    }
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
        let config = url.parse::<postgres::Config>().db()?;
        let connector = Connector::new(config, &options)?;
        PgStore::from_pool(Pool::new(Box::new(move || connector.connect()), options))
    }

    /// A store on `pool`, after opening one connection.
    pub(crate) fn from_pool(pool: Pool) -> Result<PgStore> {
        let mut store = PgStore { pool, vectors: false };
        let vectors: bool = {
            let mut c = store.pool.get()?;
            c.client.query_one("SELECT to_regclass('embeddings') IS NOT NULL", &[]).db()?.try_get(0).db()?
        };
        store.vectors = vectors;
        Ok(store)
    }

    /// `Ok` where vectors are stored and searched; otherwise [`Error::Unsupported`].
    fn vectors_or_unsupported(&self) -> Result<()> {
        if self.vectors {
            Ok(())
        } else {
            Err(Error::Unsupported("vectors need PostgreSQL with the vector extension (migration 9)".into()))
        }
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

    /// Versions from rows of (tx, receipt_hash, asof, withdrawn).
    fn version_rows(rows: &[postgres::Row], cell_id: &str) -> Result<Vec<Version>> {
        rows.iter().map(|r| version_row(r, cell_id)).collect()
    }

    /// A version (and its receipt's bytes) from a query answering
    /// (tx, receipt_hash, asof, withdrawn, bytes) for (cell_id, asof).
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
            Ok(Some((version_row(&row, cell_id)?, row.try_get(4).db()?)))
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

/// A version from a row of (tx, receipt_hash, asof, withdrawn, ...).
fn version_row(r: &postgres::Row, cell_id: &str) -> Result<Version> {
    Ok(Version {
        cell_id: cell_id.to_string(),
        tx: r.try_get(0).db()?,
        receipt_hash: r.try_get(1).db()?,
        asof: r.try_get(2).db()?,
        withdrawn: r.try_get(3).db()?,
    })
}

/// A version from a row of (tx, receipt_hash, asof, ...) whose withdrawn versions are left out
/// of the query (a member or a value of a slice).
fn valued_version_row(r: &postgres::Row, cell_id: &str) -> Result<Version> {
    Ok(Version {
        cell_id: cell_id.to_string(),
        tx: r.try_get(0).db()?,
        receipt_hash: r.try_get(1).db()?,
        asof: r.try_get(2).db()?,
        withdrawn: false,
    })
}

/// A member from a row of (tx, receipt_hash, asof, bytes, ...): a slice's member, never withdrawn.
fn member_row(r: &postgres::Row, cell_id: &str) -> Result<SliceMember> {
    Ok(SliceMember { version: valued_version_row(r, cell_id)?, receipt: r.try_get(3).db()? })
}

/// A version-slice member from a row of (tx, receipt_hash, asof, withdrawn, bytes).
fn versions_between_row(r: &postgres::Row, cell_id: &str) -> Result<SliceMember> {
    Ok(SliceMember { version: version_row(r, cell_id)?, receipt: r.try_get(4).db()? })
}

/// A cell's arguments from the "name:type:value" items of the typed-argument index
/// (`cell_args` holds every argument's type and lexical form).
fn cell_args(items: Vec<String>) -> Result<datumesh_core::Args> {
    let bad =
        |what: &str| Error::Invalid(datumesh_core::Error::new(ErrorCode::CanonType, "$", format!("a stored {what}")));
    let mut args = datumesh_core::Args::new();
    for a in items {
        let mut parts = a.splitn(3, ':');
        let (Some(name), Some(t), Some(v)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad("argument"));
        };
        let ty = Type::from_name(t).ok_or_else(|| bad("type"))?;
        args.insert(name.to_string(), datumesh_core::Arg::Value(Scalar::from_lexical(ty, v)?));
    }
    Ok(args)
}

/// A member's value and cell from a row of (tx, receipt_hash, asof, value_type, value, ns,
/// fn, fn_hash, "name:type:value" of each argument, ...): the cell is rebuilt from the
/// typed-argument index.
fn value_row(r: &postgres::Row, cell_id: &str) -> Result<SliceValue> {
    let bad =
        |what: &str| Error::Invalid(datumesh_core::Error::new(ErrorCode::CanonType, "$", format!("a stored {what}")));
    let (ty, value): (String, String) = (r.try_get(3).db()?, r.try_get(4).db()?);
    let ty = Type::from_name(&ty).ok_or_else(|| bad("type"))?;
    let args = cell_args(r.try_get::<_, Vec<String>>(8).db()?)?;
    let cell = Cell { args, fn_hash: r.try_get(7).db()?, fn_name: r.try_get(6).db()?, ns: r.try_get(5).db()? };
    let sort_key: Vec<u8> = r.try_get(9).db()?;
    Ok(SliceValue {
        version: valued_version_row(r, cell_id)?,
        value: Scalar::from_lexical(ty, &value)?,
        cell,
        sort_key,
    })
}

/// A cell from a row of (ns, fn, fn_hash, "name:type:value" of each argument).
fn cell_row(r: &postgres::Row) -> Result<Cell> {
    Ok(Cell {
        args: cell_args(r.try_get::<_, Vec<String>>(3).db()?)?,
        fn_hash: r.try_get(2).db()?,
        fn_name: r.try_get(1).db()?,
        ns: r.try_get(0).db()?,
    })
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

/// The host of a fully qualified function name (`host/name`): what precedes the last `/`.
fn name_host(fn_name: &str) -> &str {
    fn_name.rsplit_once('/').map_or("", |(h, _)| h)
}

/// The namespace in `row`, whose columns from `at` are keydoc, validated_at, suspended_at,
/// consecutive_failures, last_checked_at.
fn namespace_row(host: &str, row: &postgres::Row, at: usize) -> Result<Namespace> {
    let failures: i32 = row.try_get(at + 3).db()?;
    Ok(Namespace {
        host: host.to_string(),
        keydoc: row.try_get(at).db()?,
        validated_at: row.try_get(at + 1).db()?,
        suspended_at: row.try_get(at + 2).db()?,
        consecutive_failures: u32::try_from(failures).unwrap_or(0),
        last_checked_at: row.try_get(at + 4).db()?,
    })
}

impl PgStore {
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
        let out = self.run(op, |c| {
            let lock = c.stmt(Q::Lock)?;
            let get = c.stmt(Q::NamespaceGet)?;
            let put = c.stmt(Q::NamespaceCheck)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"namespaces", &host]).db()?;
            let Some(row) = t.query_opt(&get, &[&host]).db()? else { return Ok(None) };
            let mut ns = namespace_row(host, &row, 0)?;
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
            let failures = i32::try_from(ns.consecutive_failures).unwrap_or(i32::MAX);
            t.execute(&put, &[&host, &failures, &checked_at, &ns.suspended_at]).db()?;
            t.commit().db()?;
            Ok(Some(ns))
        })?;
        tracing::debug!(target: "datumesh_store", host, checked_at, ok = threshold.is_none(), "namespace check");
        Ok(out)
    }
}

/// Items per statement of `latest_many` and `receipts_many` (longer inputs are split).
const MANY: usize = 10_000;

/// The receipt's verdict against the keys of its signer host, as `put_receipt` gives it:
/// `None` when it verifies.
fn verdict(bytes: &[u8], keys: &Keys) -> Option<datumesh_core::Error> {
    let out = verify(bytes, keys, None, None);
    if out.ok {
        return None;
    }
    Some(
        out.error()
            .unwrap_or_else(|| datumesh_core::Error::new(ErrorCode::Signature, "$", "the receipt did not verify")),
    )
}

/// `f` over `items` on up to `available_parallelism()` threads; the results in item order.
fn parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(items.len());
    if threads <= 1 {
        return items.iter().map(f).collect();
    }
    let size = items.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(size)
            .map(|part| {
                let f = &f;
                s.spawn(move || part.iter().map(f).collect::<Vec<R>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
    })
}

/// A signer host's namespace as `put_receipt` and `put_version` read it: its key document
/// (the keys it verifies with) and whether it is suspended, and since when.
struct HostState {
    keys: Keys,
    suspended_at: Option<String>,
}

/// `put_cells` on one connection, in one transaction: the items in order, with the checks
/// and the errors of `put_receipt` and `put_version` (see `Store::put_cells`).
fn put_cells_in(c: &mut Conn, items: &[CellWrite<'_>], prepared: Vec<Result<Parsed>>) -> Result<Vec<String>> {
    let version_cells = || {
        items
            .iter()
            .filter_map(|w| w.version)
            .filter(|(cell, tx)| check_hex(cell, 64, "cell_id").is_ok() && check_instant(tx, "tx").is_ok())
    };
    let st_lock = c.stmt(Q::VersionLocksMany)?;
    let st_get = c.stmt(Q::ReceiptsGetMany)?;
    let st_ns = c.stmt(Q::NamespacesGetMany)?;
    let st_latest = c.stmt(Q::VersionsLatestTxMany)?;
    let st_put = c.stmt(Q::ReceiptsPutMany)?;
    let st_edges = c.stmt(Q::EdgesPutMany)?;
    let st_slice_edges = c.stmt(Q::SliceEdgesPutMany)?;
    let st_versions = c.stmt(Q::VersionsPutMany)?;
    let mut t = c.client.transaction().db()?;

    // The version locks, in item order (as put_version takes them; one statement).
    let mut locked: Vec<&str> = Vec::new();
    for (cell, _) in version_cells() {
        if !locked.contains(&cell) {
            locked.push(cell);
        }
    }
    if !locked.is_empty() {
        t.execute(&st_lock, &[&locked]).db()?;
    }

    // What the store holds for the call: the receipts (by hash), the namespaces of the signer
    // hosts and the latest tx of the version cells, all read after the locks.
    let parsed: Vec<&Parsed> = prepared.iter().filter_map(|p| p.as_ref().ok()).collect();
    let mut hashes: Vec<&str> = parsed.iter().map(|p| p.hash.as_str()).collect();
    hashes.sort_unstable();
    hashes.dedup();
    let mut stored: HashMap<String, Vec<u8>> = HashMap::new();
    if !hashes.is_empty() {
        for row in t.query(&st_get, &[&hashes]).db()? {
            stored.insert(row.try_get(0).db()?, row.try_get(1).db()?);
        }
    }
    let mut hosts: Vec<&str> = parsed.iter().map(|p| p.host.as_str()).collect();
    hosts.sort_unstable();
    hosts.dedup();
    let mut namespaces: HashMap<String, HostState> = HashMap::new();
    if !hosts.is_empty() {
        for row in t.query(&st_ns, &[&hosts]).db()? {
            let host: String = row.try_get(0).db()?;
            let doc: Vec<u8> = row.try_get(1).db()?;
            let suspended_at: Option<String> = row.try_get(2).db()?;
            let mut keys = Keys::new();
            keys.insert_response(&host, 200, Some(&doc));
            namespaces.insert(host, HostState { keys, suspended_at });
        }
    }
    let mut cells: Vec<&str> = version_cells().map(|(cell, _)| cell).collect();
    cells.sort_unstable();
    cells.dedup();
    let mut latest: HashMap<String, String> = HashMap::new();
    if !cells.is_empty() {
        for row in t.query(&st_latest, &[&cells]).db()? {
            latest.insert(row.try_get(0).db()?, row.try_get(1).db()?);
        }
    }

    // The signatures, on threads: every item not already stored with the same bytes (the
    // others are not checked, as put_receipt's idempotence says). A namespace's keys are
    // those of the stored key document; none stored is a host without keys.
    let empty = Keys::new();
    let keys_of = |host: &str| namespaces.get(host).map_or(&empty, |h| &h.keys);
    let jobs: Vec<usize> = (0..items.len())
        .filter(|&i| match &prepared[i] {
            Ok(p) => stored.get(&p.hash) != Some(&p.canonical),
            Err(_) => false,
        })
        .collect();
    let mut verdicts: Vec<Option<Option<datumesh_core::Error>>> = vec![None; items.len()];
    let checked = parallel(&jobs, |&i| {
        let host = prepared[i].as_ref().map_or("", |p| p.host.as_str());
        verdict(items[i].receipt, keys_of(host))
    });
    for (i, v) in jobs.into_iter().zip(checked) {
        verdicts[i] = Some(v);
    }

    // In item order, as the loop of put_receipt and put_version would run them, against the
    // state of the store plus the items before. The first failure is the call's error.
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    let mut new: Vec<Parsed> = Vec::new();
    let mut versions: Vec<(&str, &str, String)> = Vec::new();
    for (i, pr) in prepared.into_iter().enumerate() {
        let p = pr?;
        let host_state = namespaces.get(&p.host);
        let suspended = |host: &str| -> Result<()> {
            match host_state.and_then(|h| h.suspended_at.as_ref()) {
                // No new writes under a suspended namespace.
                Some(since) => Err(Error::NamespaceSuspended { host: host.to_string(), suspended_at: since.clone() }),
                None => Ok(()),
            }
        };
        // put_receipt: the stored copy with the same bytes is the receipt (no checks).
        let is_stored = stored.get(&p.hash).is_some_and(|b| *b == p.canonical);
        if !is_stored {
            suspended(&p.host)?;
            match verdicts[i].take() {
                Some(Some(e)) => return Err(Error::Invalid(e)),
                Some(None) => {}
                None => unreachable!("an item not stored is checked"),
            }
            if stored.contains_key(&p.hash) {
                return Err(Error::Conflict { hash: p.hash.clone() });
            }
            stored.insert(p.hash.clone(), p.canonical.clone());
        }
        // put_version, for the item's version.
        if let Some((cell, tx)) = items[i].version {
            check_hex(cell, 64, "cell_id")?;
            check_instant(tx, "tx")?;
            suspended(&p.host)?;
            if p.cell_id != cell {
                return Err(Error::Mismatch { what: "cell_id", expected: cell.to_string(), found: p.cell_id.clone() });
            }
            if p.tx != tx {
                return Err(Error::Mismatch { what: "tx", expected: tx.to_string(), found: p.tx.clone() });
            }
            if let Some(l) = latest.get(cell) {
                if tx <= l.as_str() {
                    return Err(Error::NotLater {
                        what: "version",
                        key: cell.to_string(),
                        given: tx.to_string(),
                        latest: l.clone(),
                    });
                }
            }
            latest.insert(cell.to_string(), tx.to_string());
            versions.push((cell, tx, p.hash.clone()));
        }
        out.push(p.hash.clone());
        if !is_stored {
            new.push(p);
        }
    }

    // The writes. A receipt another transaction stored meanwhile must hold the same bytes.
    if !new.is_empty() {
        let hashes: Vec<&str> = new.iter().map(|p| p.hash.as_str()).collect();
        let bytes: Vec<&[u8]> = new.iter().map(|p| p.canonical.as_slice()).collect();
        let cells: Vec<&str> = new.iter().map(|p| p.cell_id.as_str()).collect();
        let txs: Vec<&str> = new.iter().map(|p| p.tx.as_str()).collect();
        let asofs: Vec<&str> = new.iter().map(|p| p.asof.as_str()).collect();
        let fns: Vec<&str> = new.iter().map(|p| p.fn_hash.as_str()).collect();
        let pubs: Vec<&str> = new.iter().map(|p| p.publisher.as_str()).collect();
        let rows = t.query(&st_put, &[&hashes, &bytes, &cells, &txs, &asofs, &fns, &pubs]).db()?;
        let mut got: HashSet<String> = HashSet::new();
        for row in &rows {
            got.insert(row.try_get(0).db()?);
        }
        let missing: Vec<&str> = hashes.iter().copied().filter(|h| !got.contains(*h)).collect();
        if !missing.is_empty() {
            let mut held: HashMap<String, Vec<u8>> = HashMap::new();
            for row in t.query(&st_get, &[&missing]).db()? {
                held.insert(row.try_get(0).db()?, row.try_get(1).db()?);
            }
            for p in new.iter().filter(|p| missing.contains(&p.hash.as_str())) {
                if held.get(&p.hash) != Some(&p.canonical) {
                    return Err(Error::Conflict { hash: p.hash.clone() });
                }
            }
        }
        let edges: Vec<(&str, &str)> =
            new.iter().flat_map(|p| p.inputs.iter().map(move |i| (p.hash.as_str(), i.as_str()))).collect();
        if !edges.is_empty() {
            let (r, i): (Vec<&str>, Vec<&str>) = edges.into_iter().unzip();
            t.execute(&st_edges, &[&r, &i]).db()?;
        }
        let slice_edges: Vec<(&str, &str)> =
            new.iter().flat_map(|p| p.entries.iter().map(move |e| (p.hash.as_str(), e.as_str()))).collect();
        if !slice_edges.is_empty() {
            let (r, e): (Vec<&str>, Vec<&str>) = slice_edges.into_iter().unzip();
            t.execute(&st_slice_edges, &[&r, &e]).db()?;
        }
    }
    if !versions.is_empty() {
        let cells: Vec<&str> = versions.iter().map(|v| v.0).collect();
        let txs: Vec<&str> = versions.iter().map(|v| v.1).collect();
        let hashes: Vec<&str> = versions.iter().map(|v| v.2.as_str()).collect();
        t.execute(&st_versions, &[&cells, &txs, &hashes]).db()?;
    }
    t.commit().db()?;
    Ok(out)
}

/// The version of `cell_id` in a row of `VersionLatestMany` (tx, receipt hash, asof), `None`
/// when the cell has none.
fn latest_row(r: &postgres::Row, cell_id: &str) -> Result<Option<Version>> {
    let Some(tx) = r.try_get::<_, Option<String>>(0).db()? else { return Ok(None) };
    Ok(Some(Version {
        cell_id: cell_id.to_string(),
        tx,
        receipt_hash: r.try_get(1).db()?,
        asof: r.try_get(2).db()?,
        withdrawn: r.try_get(3).db()?,
    }))
}

/// The method names a class binds (K5, as `bind_class` does): `<class name>/<m>` for each
/// method the class adds or overrides (its function hash differs from its parent's), with the
/// function hash.
fn own_method_bindings(
    t: &mut postgres::Transaction<'_>,
    get: &postgres::Statement,
    class_name: &str,
    class_hash: &str,
) -> Result<Vec<(String, String)>> {
    let Some(row) = t.query_opt(get, &[&class_hash]).db()? else {
        return Err(Error::NotFound { what: "class", key: class_hash.to_string() });
    };
    let flat = ClassDescriptor::from_json(&row.try_get::<_, Vec<u8>>(0).db()?)?;
    let parent = match &flat.parent {
        Some(p) => match t.query_opt(get, &[p]).db()? {
            Some(row) => Some(ClassDescriptor::from_json(&row.try_get::<_, Vec<u8>>(0).db()?)?.methods),
            None => None,
        },
        None => None,
    };
    let methods: Vec<(String, String)> = flat.methods.into_iter().collect();
    Ok(methods
        .iter()
        .filter(|(m, h)| parent.as_ref().and_then(|pm| pm.get(m)) != Some(h))
        .map(|(m, h)| (format!("{class_name}/{m}"), h.clone()))
        .collect())
}

impl Store for PgStore {
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String> {
        // spec-0.4.0's descriptor: `signature.order` and argument `extent` allowed.
        let d = FunctionDescriptor::from_json(bytes)?;
        let hash = d.hash();
        let canonical = d.canonical();
        // The descriptor and its inputs' index (0.5.1), in one transaction.
        self.run("put_descriptor", |c| {
            let st = c.stmt(Q::DescriptorPut)?;
            let inputs_put = c.stmt(Q::DescriptorInputsPut)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&st, &[&hash, &canonical]).db()?;
            if !d.inputs.is_empty() {
                t.execute(&inputs_put, &[&hash, &d.inputs]).db()?;
            }
            t.commit().db()?;
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
        let cell_id = p.cell_id.clone();
        let host = p.host.clone();
        let (entries, inputs) = (p.entries.clone(), p.inputs.clone());
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
                // No new writes under a suspended namespace.
                if let Some(since) = row.try_get::<_, Option<String>>(2).db()? {
                    return Err(Error::NamespaceSuspended { host: host.clone(), suspended_at: since });
                }
                let doc: Vec<u8> = row.try_get(0).db()?;
                keys.insert_response(&host, 200, Some(&doc));
            }
            let out = verify(bytes, &keys, None, None);
            if !out.ok {
                return Err(Error::Invalid(out.error().unwrap_or_else(|| {
                    datumesh_core::Error::new(ErrorCode::Signature, "$", "the receipt did not verify")
                })));
            }
            let n = t.execute(&put, &[&hash, &canonical, &cell_id, &p.tx, &p.asof, &p.fn_hash, &p.publisher]).db()?;
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
            let get = c.stmt(Q::ReceiptGet)?;
            let ns = c.stmt(Q::NamespaceGet)?;
            let latest = c.stmt(Q::VersionLatestTx)?;
            let put = c.stmt(Q::VersionPut)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"versions", &cell_id]).db()?;
            let Some(row) = t.query_opt(&meta, &[&receipt_hash]).db()? else {
                return Err(Error::NotFound { what: "receipt", key: receipt_hash.to_string() });
            };
            // No new versions of a receipt signed by a suspended namespace.
            let stored: Vec<u8> = t.query_one(&get, &[&receipt_hash]).db()?.try_get(0).db()?;
            let signer = parse_receipt(&stored)?.host;
            if let Some(row) = t.query_opt(&ns, &[&signer]).db()? {
                if let Some(since) = row.try_get::<_, Option<String>>(2).db()? {
                    return Err(Error::NamespaceSuspended { host: signer, suspended_at: since });
                }
            }
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

    fn latest_many(&self, cell_ids: &[&str]) -> Result<Vec<Option<Version>>> {
        for id in cell_ids {
            check_hex(id, 64, "cell_id")?;
        }
        let mut out = Vec::with_capacity(cell_ids.len());
        for part in cell_ids.chunks(MANY) {
            let versions = self.run("latest_many", |c| {
                let st = c.stmt(Q::VersionLatestMany)?;
                let rows = c.client.query(&st, &[&part]).db()?;
                rows.iter().zip(part).map(|(r, id)| latest_row(r, id)).collect::<Result<Vec<_>>>()
            })?;
            out.extend(versions);
        }
        Ok(out)
    }

    fn receipts_many(&self, hashes: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut out = Vec::with_capacity(hashes.len());
        for part in hashes.chunks(MANY) {
            // A malformed hash has no receipt (as `receipt` answers): it is not sent.
            let valid: Vec<&str> = part.iter().copied().filter(|h| is_hash(h)).collect();
            let found: Vec<Option<Vec<u8>>> = if valid.is_empty() {
                Vec::new()
            } else {
                self.run("receipts_many", |c| {
                    let st = c.stmt(Q::ReceiptsMany)?;
                    let rows = c.client.query(&st, &[&valid]).db()?;
                    rows.iter().map(|r| r.try_get::<_, Option<Vec<u8>>>(0).db()).collect::<Result<Vec<_>>>()
                })?
            };
            let mut found = found.into_iter();
            for h in part {
                out.push(if is_hash(h) { found.next().flatten() } else { None });
            }
        }
        Ok(out)
    }

    fn put_cells(&self, items: &[CellWrite<'_>]) -> Result<Vec<String>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        // Parsing (and so the first malformed receipt's error) is in item order below.
        let prepared = parallel(items, |w| parse_receipt(w.receipt));
        let hashes = self.run("put_cells", |c| put_cells_in(c, items, prepared))?;
        tracing::debug!(target: "datumesh_store", items = items.len(), "put_cells");
        Ok(hashes)
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
            Ok(Some(namespace_row(host, &row, 0)?))
        })
    }

    fn namespaces(&self) -> Result<Vec<Namespace>> {
        self.run("namespaces", |c| {
            let st = c.stmt(Q::NamespaceList)?;
            let mut out = Vec::new();
            for row in c.client.query(&st, &[]).db()? {
                let host: String = row.try_get(0).db()?;
                out.push(namespace_row(&host, &row, 1)?);
            }
            Ok(out)
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
        self.run("bind_name", |c| {
            let lock = c.stmt(Q::Lock)?;
            let exists = c.stmt(Q::DescriptorExists)?;
            let ns = c.stmt(Q::NamespaceGet)?;
            let latest = c.stmt(Q::NameLatest)?;
            let put = c.stmt(Q::NamePut)?;
            let class_bound = c.stmt(Q::ClassNameBound)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"names", &fn_name]).db()?;
            // No new bindings under a suspended namespace.
            if let Some(row) = t.query_opt(&ns, &[&name_host(fn_name)]).db()? {
                if let Some(since) = row.try_get::<_, Option<String>>(2).db()? {
                    return Err(Error::NamespaceSuspended {
                        host: name_host(fn_name).to_string(),
                        suspended_at: since,
                    });
                }
            }
            // One namespace of names: a class name is not a function name.
            let class: bool = t.query_one(&class_bound, &[&fn_name]).db()?.try_get(0).db()?;
            if class {
                return Err(Error::NameTaken { name: fn_name.to_string(), bound_as: "class" });
            }
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
            // A computed version states a value: a withdrawal is refused (0.6.0).
            if row.try_get::<_, bool>(3).db()? {
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
            rows.iter().map(|r| versions_between_row(r, cell_id)).collect()
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
        let pattern = Pattern::of(&v)?;
        self.run("slice_withdrawn_latest", |c| pattern.withdrawn_latest(c, asof))
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

    fn computed_cells(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<Cell>> {
        check_instant(asof, "asof")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let pattern = Pattern::of(&slice.to_value())?;
        self.run("computed_cells", |c| pattern.computed_cells(c, asof, limit))
    }

    fn has_bindings_under(&self, namespace: &str) -> Result<bool> {
        let prefix = format!("{namespace}/");
        self.run("has_bindings_under", |c| {
            let st = c.stmt(Q::HasBindingsUnder)?;
            c.client.query_one(&st, &[&prefix]).db()?.try_get(0).db()
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
        self.run("put_class", |c| {
            // No new writes under a suspended namespace: the steward's (16.2).
            let ns = c.stmt(Q::NamespaceGet)?;
            if let Some(row) = c.client.query_opt(&ns, &[&host]).db()? {
                if let Some(since) = row.try_get::<_, Option<String>>(2).db()? {
                    return Err(Error::NamespaceSuspended { host: host.clone(), suspended_at: since });
                }
            }
            let put = c.stmt(Q::ClassPut)?;
            c.client.execute(&put, &[&hash, &canonical]).db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", class_hash = %hash, "put_class");
        Ok(hash)
    }

    fn class(&self, class_hash: &str) -> Result<Option<Vec<u8>>> {
        check_hex(class_hash, 64, "class_hash")?;
        self.run("class", |c| get_bytes(c, Q::ClassGet, class_hash))
    }

    fn bind_class(&self, class_name: &str, class_hash: &str, tx: &str) -> Result<()> {
        check_function_name(class_name, "class_name")?;
        check_hex(class_hash, 64, "class_hash")?;
        check_instant(tx, "tx")?;
        self.run("bind_class", |c| {
            let lock = c.stmt(Q::Lock)?;
            let ns = c.stmt(Q::NamespaceGet)?;
            let get = c.stmt(Q::ClassGet)?;
            let exists = c.stmt(Q::DescriptorExists)?;
            let name_bound = c.stmt(Q::NameBound)?;
            let class_bound = c.stmt(Q::ClassNameBound)?;
            let class_latest = c.stmt(Q::ClassBindingLatest)?;
            let latest = c.stmt(Q::NameLatest)?;
            let class_put = c.stmt(Q::ClassBindingPut)?;
            let name_put = c.stmt(Q::NamePut)?;
            let mut t = c.client.transaction().db()?;
            // Every name this binding makes is locked before it is read: the class name, then
            // its own method names (the flattened descriptor's, in JCS order).
            t.execute(&lock, &[&"names", &class_name]).db()?;
            // No new bindings under a suspended namespace: the host of the class name.
            let host = name_host(class_name);
            if let Some(row) = t.query_opt(&ns, &[&host]).db()? {
                if let Some(since) = row.try_get::<_, Option<String>>(2).db()? {
                    return Err(Error::NamespaceSuspended { host: host.to_string(), suspended_at: since });
                }
            }
            let Some(row) = t.query_opt(&get, &[&class_hash]).db()? else {
                return Err(Error::NotFound { what: "class", key: class_hash.to_string() });
            };
            let flat = ClassDescriptor::from_json(&row.try_get::<_, Vec<u8>>(0).db()?)?;
            let methods: Vec<(String, String)> = flat.methods.into_iter().collect();
            // K5: only the methods this class adds or overrides are bound under its name: those
            // whose function hash the parent's flattened descriptor does not give.
            let parent = match &flat.parent {
                Some(p) => match t.query_opt(&get, &[p]).db()? {
                    Some(row) => Some(ClassDescriptor::from_json(&row.try_get::<_, Vec<u8>>(0).db()?)?.methods),
                    None => None,
                },
                None => None,
            };
            let own: Vec<(String, String)> =
                methods.iter().filter(|(m, h)| parent.as_ref().and_then(|pm| pm.get(m)) != Some(h)).cloned().collect();
            let own_names: Vec<String> = own.iter().map(|(m, _)| format!("{class_name}/{m}")).collect();
            for name in &own_names {
                t.execute(&lock, &[&"names", &name.as_str()]).db()?;
            }
            // Every method's function descriptor is stored.
            for (_, h) in &methods {
                let found: bool = t.query_one(&exists, &[h]).db()?.try_get(0).db()?;
                if !found {
                    return Err(Error::NotFound { what: "descriptor", key: h.clone() });
                }
            }
            // One namespace of names: the class name is not a function name, and no method
            // name is a class name.
            let function: bool = t.query_one(&name_bound, &[&class_name]).db()?.try_get(0).db()?;
            if function {
                return Err(Error::NameTaken { name: class_name.to_string(), bound_as: "function" });
            }
            for name in &own_names {
                let class: bool = t.query_one(&class_bound, &[&name.as_str()]).db()?.try_get(0).db()?;
                if class {
                    return Err(Error::NameTaken { name: name.clone(), bound_as: "class" });
                }
            }
            // Later than the latest binding of the class name and of each method name.
            if let Some(row) = t.query_opt(&class_latest, &[&class_name]).db()? {
                let l: String = row.try_get(0).db()?;
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
                if let Some(row) = t.query_opt(&latest, &[&name.as_str()]).db()? {
                    let l: String = row.try_get(0).db()?;
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
            t.execute(&class_put, &[&class_name, &tx, &class_hash]).db()?;
            for ((_, h), name) in own.iter().zip(&own_names) {
                t.execute(&name_put, &[&name.as_str(), &tx, &h.as_str()]).db()?;
            }
            t.commit().db()?;
            Ok(())
        })?;
        tracing::debug!(target: "datumesh_store", class_name, class_hash, tx, "bind_class");
        Ok(())
    }

    fn close_binding(&self, name: &str, at: &str) -> Result<()> {
        check_function_name(name, "name")?;
        check_instant(at, "at")?;
        self.run("close_binding", |c| {
            let lock = c.stmt(Q::Lock)?;
            let ns = c.stmt(Q::NamespaceGet)?;
            let class_bound = c.stmt(Q::ClassNameBound)?;
            let name_latest = c.stmt(Q::NameLatestRow)?;
            let class_latest = c.stmt(Q::ClassBindingLatestRow)?;
            let name_close = c.stmt(Q::NameClose)?;
            let class_close = c.stmt(Q::ClassBindingClose)?;
            let get = c.stmt(Q::ClassGet)?;
            let mut t = c.client.transaction().db()?;
            t.execute(&lock, &[&"names", &name]).db()?;
            // No new writes under a suspended namespace: the host of the name.
            let host = name_host(name);
            if let Some(row) = t.query_opt(&ns, &[&host]).db()? {
                if let Some(since) = row.try_get::<_, Option<String>>(2).db()? {
                    return Err(Error::NamespaceSuspended { host: host.to_string(), suspended_at: since });
                }
            }
            let class: bool = t.query_one(&class_bound, &[&name]).db()?.try_get(0).db()?;
            if class {
                // The class name, then the method names its binding bound and that are still bound.
                let row = t.query_opt(&class_latest, &[&name]).db()?.ok_or_else(|| unbound(name))?;
                let (tx, class_hash): (String, Option<String>) = (row.try_get(0).db()?, row.try_get(1).db()?);
                let class_hash = class_hash.ok_or_else(|| unbound(name))?;
                later_than("name", name, at, &tx)?;
                let methods = own_method_bindings(&mut t, &get, name, &class_hash)?;
                let mut closing: Vec<String> = Vec::new();
                for (m, _) in &methods {
                    t.execute(&lock, &[&"names", &m.as_str()]).db()?;
                    if let Some(row) = t.query_opt(&name_latest, &[&m.as_str()]).db()? {
                        let (mtx, mhash): (String, Option<String>) = (row.try_get(0).db()?, row.try_get(1).db()?);
                        if mhash.is_some() {
                            later_than("name", m, at, &mtx)?;
                            closing.push(m.clone());
                        }
                    }
                }
                t.execute(&class_close, &[&name, &at]).db()?;
                for m in &closing {
                    t.execute(&name_close, &[&m.as_str(), &at]).db()?;
                }
            } else {
                let row = t.query_opt(&name_latest, &[&name]).db()?.ok_or_else(|| unbound(name))?;
                let (tx, fn_hash): (String, Option<String>) = (row.try_get(0).db()?, row.try_get(1).db()?);
                fn_hash.ok_or_else(|| unbound(name))?;
                later_than("name", name, at, &tx)?;
                t.execute(&name_close, &[&name, &at]).db()?;
            }
            t.commit().db()?;
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
            let st = c.stmt(Q::ClassBindingAsof)?;
            let Some(row) = c.client.query_opt(&st, &[&class_name, &asof]).db()? else { return Ok(None) };
            // A closed binding (0.6.0) has no class hash: the name is unbound from its tx on.
            let Some(class_hash) = row.try_get::<_, Option<String>>(0).db()? else { return Ok(None) };
            Ok(Some(ClassBinding { class_hash, tx: row.try_get(1).db()? }))
        })
    }

    fn class_names_bound_to(&self, class_hash: &str, at: &str) -> Result<Vec<String>> {
        check_hex(class_hash, 64, "class_hash")?;
        check_instant(at, "at")?;
        self.run("class_names_bound_to", |c| {
            let st = c.stmt(Q::ClassNamesBound)?;
            let rows = c.client.query(&st, &[&class_hash, &at]).db()?;
            rows.iter().map(|r| r.try_get(0).db()).collect()
        })
    }

    fn resolve_binding(&self, fn_name: &str, asof: &str) -> Result<Option<Binding>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_binding", |c| {
            let st = c.stmt(Q::BindingAsof)?;
            let Some(row) = c.client.query_opt(&st, &[&fn_name, &asof]).db()? else { return Ok(None) };
            // A closed binding (0.6.0) has no function hash: the name is unbound from its tx on.
            let Some(fn_hash) = row.try_get::<_, Option<String>>(0).db()? else { return Ok(None) };
            Ok(Some(Binding { fn_hash, tx: row.try_get(1).db()? }))
        })
    }

    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>> {
        check_function_name(fn_name, "fn_name")?;
        check_instant(asof, "asof")?;
        self.run("resolve_name", |c| {
            let st = c.stmt(Q::NameAsof)?;
            let row = c.client.query_opt(&st, &[&fn_name, &asof]).db()?;
            // NULL for a closed binding (0.6.0): the name means nothing from then on.
            Ok(match row {
                Some(r) => r.try_get::<_, Option<String>>(0).db()?,
                None => None,
            })
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
        self.run("upsert_key_values", |c| {
            let mut t = c.client.transaction().db()?;
            let mut ids = Vec::with_capacity(values.len());
            for v in values {
                let before = t
                    .query_opt("SELECT label, value FROM key_values WHERE owner = $1 AND key = $2 AND value = $3", &[&owner, &key, &v.value])
                    .db()?
                    .map(|r| -> Result<(String, String)> { Ok((r.try_get(0).db()?, r.try_get(1).db()?)) })
                    .transpose()?;
                let id: i64 = t
                    .query_one(
                        "INSERT INTO key_values (owner, key, value, label, aliases) VALUES ($1, $2, $3, $4, $5)
                         ON CONFLICT (owner, key, value) DO UPDATE SET label = EXCLUDED.label, aliases = EXCLUDED.aliases
                         RETURNING id",
                        &[&owner, &key, &v.value, &v.label, &v.aliases],
                    )
                    .db()?
                    .try_get(0)
                    .db()?;
                if let Some((label, value)) = before {
                    if self.vectors && value_text(&label, &value) != value_text(&v.label, &v.value) {
                        t.execute("DELETE FROM embeddings WHERE ref_kind = 'value' AND ref_id = $1", &[&id]).db()?;
                    }
                }
                ids.push(id);
            }
            t.commit().db()?;
            Ok(ids)
        })
    }

    fn refresh_value_counts(&self) -> Result<u64> {
        self.run("refresh_value_counts", |c| {
            c.client
                .execute(
                    "WITH counts AS (
                         SELECT e.name AS owner, a.name AS key, a.value AS value, count(*) AS n
                           FROM catalog_entries e JOIN cell_args a ON a.fn_hash = e.hash
                          WHERE e.kind = 'function'
                          GROUP BY e.name, a.name, a.value)
                     UPDATE key_values kv SET n_cells = x.n
                       FROM (SELECT v.id, COALESCE(c.n, 0) AS n
                               FROM key_values v
                               LEFT JOIN counts c ON c.owner = v.owner AND c.key = v.key AND c.value = v.value) x
                      WHERE x.id = kv.id",
                    &[],
                )
                .db()
        })
    }

    fn text_find(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let Some(words) = Words::new(query) else { return Ok(Vec::new()) };
        if limit == 0 {
            return Ok(Vec::new());
        }
        // The words (lower-cased) are $1; the prefilter's patterns follow; the LIMIT is last.
        // The scores are computed in SQL (migration 12), so each table is ordered and cut in SQL.
        let word_list: Vec<String> = words.words().to_vec();
        let entry_where = like_where(&words, &["name", "label", "description", "args"], 2);
        let value_where = like_where(&words, &["value", "label", "array_to_string(aliases, ' ')"], 2);
        let cap = limit;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("text_find", |c| {
            let mut hits = Vec::new();
            let mut params: Vec<Box<dyn postgres::types::ToSql + Sync>> = vec![Box::new(word_list.clone())];
            params.extend(entry_where.1.iter().map(|p| Box::new(p.clone()) as Box<dyn postgres::types::ToSql + Sync>));
            params.push(Box::new(limit));
            let sql = format!(
                "SELECT * FROM (SELECT id, kind, name, hash, label, description, args, examples, source, signed_by,
                        datumesh_entry_total($1::text[], name, label, description, args) AS total
                   FROM catalog_entries WHERE TRUE{}) e
                  WHERE total IS NOT NULL ORDER BY total DESC, name COLLATE \"C\" LIMIT ${}",
                entry_where.0,
                params.len()
            );
            let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = params.iter().map(|p| p.as_ref() as _).collect();
            for r in c.client.query(&sql, &refs).db()? {
                let entry = entry_from_columns(
                    &r.try_get::<_, String>(1).db()?,
                    r.try_get(2).db()?,
                    r.try_get(3).db()?,
                    r.try_get(4).db()?,
                    r.try_get(5).db()?,
                    &r.try_get::<_, String>(6).db()?,
                    &r.try_get::<_, String>(7).db()?,
                    &r.try_get::<_, String>(8).db()?,
                    r.try_get(9).db()?,
                )?;
                let total: i32 = r.try_get(10).db()?;
                hits.push(Hit::Entry { id: r.try_get(0).db()?, entry, score: words.score_of(total as u32) });
            }
            let mut params: Vec<Box<dyn postgres::types::ToSql + Sync>> = vec![Box::new(word_list.clone())];
            params.extend(value_where.1.iter().map(|p| Box::new(p.clone()) as Box<dyn postgres::types::ToSql + Sync>));
            params.push(Box::new(limit));
            let sql = format!(
                "SELECT * FROM (SELECT id, owner, key, value, label, aliases,
                        datumesh_value_total($1::text[], value, label, aliases) AS total
                   FROM key_values WHERE TRUE{}) v
                  WHERE total IS NOT NULL
                  ORDER BY total DESC, (owner || chr(1) || key || chr(1) || value) COLLATE \"C\" LIMIT ${}",
                value_where.0,
                params.len()
            );
            let refs: Vec<&(dyn postgres::types::ToSql + Sync)> = params.iter().map(|p| p.as_ref() as _).collect();
            for r in c.client.query(&sql, &refs).db()? {
                let aliases: Vec<String> = r.try_get(5).db()?;
                let (value, label): (String, String) = (r.try_get(3).db()?, r.try_get(4).db()?);
                let total: i32 = r.try_get(6).db()?;
                hits.push(Hit::Value {
                    id: r.try_get(0).db()?,
                    owner: r.try_get(1).db()?,
                    key: r.try_get(2).db()?,
                    value,
                    label,
                    aliases,
                    score: words.score_of(total as u32),
                });
            }
            Ok(rank(hits, cap))
        })
    }

    fn vector_find(&self, model: &str, vec: &[f32], limit: usize) -> Result<Vec<Hit>> {
        self.vectors_or_unsupported()?;
        let lit = vector_literal(vec)?;
        check_model(model)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.run("vector_find", |c| {
            let near = c
                .client
                .query(
                    "SELECT ref_kind, ref_id, (vec <=> $2::text::vector)::float8 FROM embeddings
                      WHERE model = $1 ORDER BY vec <=> $2::text::vector LIMIT $3",
                    &[&model, &lit, &as_i64(limit)],
                )
                .db()?;
            let mut order = Vec::with_capacity(near.len());
            for r in &near {
                order.push((r.try_get::<_, String>(0).db()?, r.try_get::<_, i64>(1).db()?, r.try_get::<_, f64>(2).db()?));
            }
            let entry_ids: Vec<i64> = order.iter().filter(|o| o.0 == "entry").map(|o| o.1).collect();
            let value_ids: Vec<i64> = order.iter().filter(|o| o.0 == "value").map(|o| o.1).collect();
            let mut entries = HashMap::new();
            for r in c.client.query("SELECT id, kind, name, hash, label, description, args, examples, source, signed_by FROM catalog_entries WHERE id = ANY($1)", &[&entry_ids]).db()? {
                let entry = entry_from_columns(
                    &r.try_get::<_, String>(1).db()?,
                    r.try_get(2).db()?,
                    r.try_get(3).db()?,
                    r.try_get(4).db()?,
                    r.try_get(5).db()?,
                    &r.try_get::<_, String>(6).db()?,
                    &r.try_get::<_, String>(7).db()?,
                    &r.try_get::<_, String>(8).db()?,
                    r.try_get(9).db()?,
                )?;
                entries.insert(r.try_get::<_, i64>(0).db()?, entry);
            }
            let mut values = HashMap::new();
            for r in c.client.query("SELECT id, owner, key, value, label, aliases FROM key_values WHERE id = ANY($1)", &[&value_ids]).db()? {
                values.insert(
                    r.try_get::<_, i64>(0).db()?,
                    (r.try_get::<_, String>(1).db()?, r.try_get::<_, String>(2).db()?, r.try_get::<_, String>(3).db()?, r.try_get::<_, String>(4).db()?, r.try_get::<_, Vec<String>>(5).db()?),
                );
            }
            let mut hits = Vec::with_capacity(order.len());
            for (kind, id, dist) in order {
                let score = 1.0 - dist;
                if kind == "entry" {
                    if let Some(entry) = entries.remove(&id) {
                        hits.push(Hit::Entry { id, entry, score });
                    }
                } else if let Some((owner, key, value, label, aliases)) = values.remove(&id) {
                    hits.push(Hit::Value { id, owner, key, value, label, aliases, score });
                }
            }
            Ok(hits)
        })
    }

    fn put_embeddings(&self, model: &str, items: &[Embedding]) -> Result<()> {
        self.vectors_or_unsupported()?;
        check_model(model)?;
        let lits: Vec<String> = items.iter().map(|i| vector_literal(&i.vec)).collect::<Result<_>>()?;
        if items.is_empty() {
            return Ok(());
        }
        self.run("put_embeddings", |c| {
            let mut t = c.client.transaction().db()?;
            for (item, lit) in items.iter().zip(&lits) {
                let (exists, what) = match item.ref_kind {
                    EmbedRef::Entry => ("SELECT 1 FROM catalog_entries WHERE id = $1", "catalog entry"),
                    EmbedRef::Value => ("SELECT 1 FROM key_values WHERE id = $1", "dictionary value"),
                };
                if t.query_opt(exists, &[&item.ref_id]).db()?.is_none() {
                    return Err(Error::NotFound { what, key: item.ref_id.to_string() });
                }
                t.execute(
                    "INSERT INTO embeddings (ref_kind, ref_id, model, vec) VALUES ($1, $2, $3, $4::text::vector)
                     ON CONFLICT (ref_kind, ref_id, model) DO UPDATE SET vec = EXCLUDED.vec",
                    &[&item.ref_kind.as_str(), &item.ref_id, &model, lit],
                )
                .db()?;
            }
            t.commit().db()?;
            Ok(())
        })
    }

    fn pending_embeddings(&self, model: &str, limit: usize) -> Result<Vec<PendingEmbedding>> {
        if !self.vectors {
            return Ok(Vec::new());
        }
        check_model(model)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.run("pending_embeddings", |c| {
            let rows = c
                .client
                .query(
                    "SELECT k, id, t FROM (
                         SELECT 'entry' AS k, e.id AS id,
                                CASE WHEN e.label = '' THEN e.description ELSE e.label || '. ' || e.description END AS t
                           FROM catalog_entries e
                          WHERE NOT EXISTS (SELECT 1 FROM embeddings m WHERE m.ref_kind = 'entry' AND m.ref_id = e.id AND m.model = $1)
                         UNION ALL
                         SELECT 'value', v.id, CASE WHEN v.label = '' THEN v.value ELSE v.label END
                           FROM key_values v
                          WHERE NOT EXISTS (SELECT 1 FROM embeddings m WHERE m.ref_kind = 'value' AND m.ref_id = v.id AND m.model = $1)
                     ) x
                     ORDER BY CASE k WHEN 'entry' THEN 0 ELSE 1 END, id LIMIT $2",
                    &[&model, &as_i64(limit)],
                )
                .db()?;
            rows.iter()
                .map(|r| {
                    let ref_kind = match r.try_get::<_, String>(0).db()?.as_str() {
                        "entry" => EmbedRef::Entry,
                        _ => EmbedRef::Value,
                    };
                    Ok(PendingEmbedding { ref_kind, ref_id: r.try_get(1).db()?, text: r.try_get(2).db()? })
                })
                .collect()
        })
    }

    fn vector_available(&self) -> bool {
        self.vectors
    }

    fn log_miss(&self, query: &str) -> Result<()> {
        let Some(q) = miss_text(query) else { return Ok(()) };
        self.run("log_miss", |c| {
            let mut t = c.client.transaction().db()?;
            t.execute("INSERT INTO find_misses (query) VALUES ($1)", &[&q]).db()?;
            // Keep the newest FIND_MISSES_CAP rows: prune the oldest beyond that.
            t.execute(
                "DELETE FROM find_misses WHERE ctid IN (SELECT ctid FROM find_misses ORDER BY at
                     LIMIT GREATEST((SELECT count(*) FROM find_misses) - $1, 0))",
                &[&as_i64(FIND_MISSES_CAP)],
            )
            .db()?;
            t.commit().db()?;
            Ok(())
        })
    }

    fn put_function_test(&self, test: &FunctionTest) -> Result<bool> {
        let (total, covered) = crate::functions::check_test(test)?;
        self.run("put_function_test", |c| {
            let st = c.stmt(Q::FunctionTestPut)?;
            let ensure = c.stmt(Q::CounterEnsure)?;
            let add = c.stmt(Q::CounterAdd)?;
            let mut t = c.client.transaction().db()?;
            let n = t
                .execute(
                    &st,
                    &[
                        &test.fn_hash,
                        &test.case,
                        &test.test_json,
                        &test.signed_by,
                        &test.last_run_at,
                        &test.last_result,
                        &test.last_detail,
                        &total,
                        &covered,
                    ],
                )
                .db()?;
            if n == 1 {
                let results = crate::functions::result_counts(test.last_result.as_deref());
                let last = test.last_run_at.as_deref();
                bump_counter(&mut t, &ensure, &add, &test.fn_hash, "", 1, results, last, (total, covered))?;
            }
            t.commit().db()?;
            Ok(n == 1)
        })
    }

    fn function_test(&self, fn_hash: &str, case: &str) -> Result<Option<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        crate::functions::check_case(case)?;
        self.run("function_test", |c| {
            let st = c.stmt(Q::FunctionTestOne)?;
            c.client.query_opt(&st, &[&fn_hash, &case]).db()?.as_ref().map(function_test_row).transpose()
        })
    }

    fn function_tests_failing(&self, fn_hash: &str, fn_name: Option<&str>, limit: usize) -> Result<Vec<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("function_tests_failing", |c| {
            let st = c.stmt(Q::FunctionTestsFailing)?;
            let rows = c.client.query(&st, &[&fn_hash, &fn_name, &limit]).db()?;
            rows.iter().map(function_test_row).collect()
        })
    }

    fn false_test_cells(&self, fn_hash: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("false_test_cells", |c| {
            let st = c.stmt(Q::FalseTestCells)?;
            let rows = c.client.query(&st, &[&fn_hash, &limit]).db()?;
            rows.iter().map(|r| -> Result<(String, Vec<u8>)> { Ok((r.try_get(0).db()?, r.try_get(1).db()?)) }).collect()
        })
    }

    fn dependants_of(&self, name: &str, at: &str, limit: usize) -> Result<Vec<(String, String)>> {
        check_function_name(name, "name")?;
        check_instant(at, "at")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("dependants_of", |c| {
            let st = c.stmt(Q::DependantsOf)?;
            let rows = c.client.query(&st, &[&name, &at, &limit]).db()?;
            rows.iter().map(|r| -> Result<(String, String)> { Ok((r.try_get(0).db()?, r.try_get(1).db()?)) }).collect()
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
        let pattern = Pattern::of(&v)?;
        self.run("slice_values_page", |c| pattern.values_page(c, asof, after_cell_id, limit))
    }

    fn catalog_entries_all(&self, limit: usize) -> Result<Vec<(CatalogKind, String, String, String)>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("catalog_entries_all", |c| {
            let st = c.stmt(Q::CatalogEntriesAll)?;
            let rows = c.client.query(&st, &[&limit]).db()?;
            rows.iter()
                .map(|r| -> Result<(CatalogKind, String, String, String)> {
                    let kind = CatalogKind::parse(&r.try_get::<_, String>(0).db()?)
                        .ok_or_else(|| invalid("catalog", "a stored entry has a bad kind"))?;
                    Ok((kind, r.try_get(1).db()?, r.try_get(2).db()?, r.try_get(3).db()?))
                })
                .collect()
        })
    }

    fn dictionary_values_ordered(&self, owner: &str, key: &str, limit: usize) -> Result<Vec<String>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("dictionary_values_ordered", |c| {
            let st = c.stmt(Q::DictionaryValuesOrdered)?;
            let rows = c.client.query(&st, &[&owner, &key, &limit]).db()?;
            rows.iter().map(|r| r.try_get::<_, String>(0).db()).collect()
        })
    }

    fn function_tests(&self, fn_hash: &str) -> Result<Vec<FunctionTest>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("function_tests", |c| {
            let st = c.stmt(Q::FunctionTestsOf)?;
            let rows = c.client.query(&st, &[&fn_hash]).db()?;
            rows.iter().map(function_test_row).collect()
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
        self.run("record_function_test", |c| {
            let st = c.stmt(Q::FunctionTestRecord)?;
            let old_st = c.stmt(Q::FunctionTestOld)?;
            let ensure = c.stmt(Q::CounterEnsure)?;
            let add = c.stmt(Q::CounterAdd)?;
            let recompute = c.stmt(Q::CounterRunRecompute)?;
            let mut t = c.client.transaction().db()?;
            let Some(old) = t.query_opt(&old_st, &[&fn_hash, &case]).db()? else { return Ok(false) };
            let old_name: Option<String> = old.try_get(0).db()?;
            let old_result: Option<String> = old.try_get(1).db()?;
            t.execute(&st, &[&run_at, &result, &detail, &fn_hash, &case]).db()?;
            // The test moves within its row: the row's counts change by the difference.
            let (before, after) =
                (crate::functions::result_counts(old_result.as_deref()), crate::functions::result_counts(Some(result)));
            let row = old_name.unwrap_or_default();
            let d: Vec<i64> = after.iter().zip(before.iter()).map(|(a, b)| a - b).collect();
            let results = [d[0], d[1], d[2], d[3]];
            bump_counter(&mut t, &ensure, &add, fn_hash, &row, 0, results, None, (0, 0))?;
            t.execute(&recompute, &[&fn_hash, &row]).db()?;
            t.commit().db()?;
            Ok(true)
        })
    }

    fn set_function_branches(&self, fn_hash: &str, total: u32, covered: u32) -> Result<()> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let (total, covered) = crate::functions::branch_counts(total, covered)?;
        self.run("set_function_branches", |c| {
            let st = c.stmt(Q::FunctionBranches)?;
            c.client.execute(&st, &[&fn_hash, &total, &covered]).db()?;
            Ok(())
        })
    }

    fn function_readers(&self, name: &str) -> Result<Vec<String>> {
        check_function_name(name, "name")?;
        self.run("function_readers", |c| get_strings(c, Q::FunctionReaders, name))
    }

    fn bound_names(&self, fn_hash: &str) -> Result<Vec<String>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("bound_names", |c| get_strings(c, Q::BoundNames, fn_hash))
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
        let cases: Vec<String> = names.iter().map(|(c, _)| c.clone()).collect();
        let fn_names: Vec<String> = names.iter().map(|(_, n)| n.clone()).collect();
        self.run("name_function_tests", |c| {
            let st = c.stmt(Q::FunctionTestName)?;
            let ensure = c.stmt(Q::CounterEnsure)?;
            let add = c.stmt(Q::CounterAdd)?;
            let recompute = c.stmt(Q::CounterRunRecompute)?;
            let mut t = c.client.transaction().db()?;
            let named = t.query(&st, &[&fn_hash, &cases, &fn_names]).db()?;
            // The rows moved from unnamed to their names, with their results: [tests, pass,
            // fail, error, not_run] per name.
            let mut moved: std::collections::BTreeMap<String, [i64; 5]> = std::collections::BTreeMap::new();
            for r in &named {
                let name: String = r.try_get(0).db()?;
                let result: Option<String> = r.try_get(1).db()?;
                let d = crate::functions::result_counts(result.as_deref());
                let e = moved.entry(name).or_insert([0; 5]);
                e[0] += 1;
                for (i, x) in d.iter().enumerate() {
                    e[i + 1] += x;
                }
            }
            let unnamed = "";
            for (name, d) in &moved {
                let results = [d[1], d[2], d[3], d[4]];
                let minus = results.map(|x| -x);
                bump_counter(&mut t, &ensure, &add, fn_hash, name, d[0], results, None, (0, 0))?;
                bump_counter(&mut t, &ensure, &add, fn_hash, unnamed, -d[0], minus, None, (0, 0))?;
                t.execute(&recompute, &[&fn_hash, name]).db()?;
            }
            if !moved.is_empty() {
                t.execute(&recompute, &[&fn_hash, &unnamed]).db()?;
            }
            t.commit().db()?;
            Ok(named.len() as u64)
        })
    }

    fn unnamed_function_tests(&self, limit: usize) -> Result<Vec<FunctionTest>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("unnamed_function_tests", |c| {
            let st = c.stmt(Q::FunctionTestsUnnamed)?;
            let rows = c.client.query(&st, &[&limit]).db()?;
            rows.iter().map(function_test_row).collect()
        })
    }

    fn function_test_summary(&self, fn_hash: &str, fn_name: Option<&str>) -> Result<TestSummary> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("function_test_summary", |c| {
            let st = c.stmt(Q::FunctionTestSummary)?;
            let r = c.client.query_one(&st, &[&fn_hash, &fn_name]).db()?;
            // The counts are bigint, the branch counts integer (at least zero): exact casts.
            let count = |i: usize| -> Result<u64> { Ok(r.try_get::<_, i64>(i).db()? as u64) };
            let total: i32 = r.try_get(6).db()?;
            let covered: i32 = r.try_get(7).db()?;
            Ok(TestSummary {
                count: count(0)?,
                pass: count(1)?,
                fail: count(2)?,
                error: count(3)?,
                not_run: count(4)?,
                last_run_at: r.try_get(5).db()?,
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
            let rows = match fn_name {
                None => {
                    let st = c.stmt(Q::FunctionTestsPage)?;
                    c.client.query(&st, &[&fn_hash, &after_case, &limit]).db()?
                }
                Some(name) => {
                    let st = c.stmt(Q::FunctionTestsPageNamed)?;
                    c.client.query(&st, &[&fn_hash, &name, &after_case, &limit]).db()?
                }
            };
            rows.iter().map(function_test_row).collect()
        })
    }

    fn stored_receipts_of(&self, ns: &str, fn_name: &str, fn_hash: &str, limit: usize) -> Result<Vec<Vec<u8>>> {
        check_hex(fn_hash, 64, "fn_hash")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("stored_receipts_of", |c| {
            let st = c.stmt(Q::StoredReceiptsOf)?;
            let rows = c.client.query(&st, &[&fn_hash, &ns, &fn_name, &limit]).db()?;
            rows.iter().map(|r| r.try_get::<_, Vec<u8>>(0).db()).collect()
        })
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
        let pattern = Pattern::of(&v)?;
        self.run("slice_page", |c| pattern.values_by_key(c, asof, after, limit))
    }

    fn slice_values_limit(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<SliceValue>> {
        check_instant(asof, "asof")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let v = slice.to_value();
        if let Some((cell_id, from, to)) = version_slice(slice, &v)? {
            return self.run("slice_values_limit", |c| {
                // The statement of slice_values, with its LIMIT: the same order (tx).
                let sql = format!("{} LIMIT $5", Q::VersionsBetweenValues.sql());
                let rows = c.client.query(sql.as_str(), &[&cell_id, &from, &to, &asof, &limit]).db()?;
                rows.iter().map(|r| value_row(r, &cell_id)).collect()
            });
        }
        let pattern = Pattern::of(&v)?;
        self.run("slice_values_limit", |c| pattern.values_limited(c, asof, limit))
    }

    fn versions_limit(&self, cell_id: &str, from: &str, to: &str, limit: usize) -> Result<Vec<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(from, "from")?;
        check_instant(to, "to")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("versions_limit", |c| {
            let st = c.stmt(Q::VersionRangeLimit)?;
            let rows = c.client.query(&st, &[&cell_id, &from, &to, &limit]).db()?;
            PgStore::version_rows(&rows, cell_id)
        })
    }

    fn stored_cell_count(&self, ns: &str, fn_name: &str, fn_hash: &str) -> Result<u64> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.run("stored_cell_count", |c| {
            let st = c.stmt(Q::StoredCellCount)?;
            let n: i64 = c.client.query_one(&st, &[&fn_hash, &ns, &fn_name]).db()?.try_get(0).db()?;
            Ok(n as u64)
        })
    }

    fn has_key_value(&self, owner: &str, key: &str, value: &str) -> Result<bool> {
        self.run("has_key_value", |c| {
            let st = c.stmt(Q::DictionaryHas)?;
            c.client.query_one(&st, &[&owner, &key, &value]).db()?.try_get(0).db()
        })
    }

    fn key_value_count(&self, owner: &str, key: &str) -> Result<u64> {
        self.run("key_value_count", |c| {
            let st = c.stmt(Q::DictionaryCount)?;
            let n: i64 = c.client.query_one(&st, &[&owner, &key]).db()?.try_get(0).db()?;
            Ok(n as u64)
        })
    }

    fn mark_key_open(&self, owner: &str, key: &str) -> Result<()> {
        if owner.is_empty() || owner.len() > 512 || key.is_empty() || key.len() > 128 {
            return Err(invalid("key", "an owner of 1 to 512 bytes and a key of 1 to 128 bytes"));
        }
        self.run("mark_key_open", |c| {
            let st = c.stmt(Q::KeyOpenPut)?;
            c.client.execute(&st, &[&owner, &key]).db()?;
            Ok(())
        })
    }

    fn open_key_count(&self) -> Result<u64> {
        self.run("open_key_count", |c| {
            let st = c.stmt(Q::OpenKeyCount)?;
            let n: i64 = c.client.query_one(&st, &[]).db()?.try_get(0).db()?;
            Ok(n as u64)
        })
    }

    fn key_is_open(&self, owner: &str, key: &str) -> Result<bool> {
        self.run("key_is_open", |c| {
            let st = c.stmt(Q::KeyIsOpen)?;
            c.client.query_one(&st, &[&owner, &key]).db()?.try_get(0).db()
        })
    }

    fn catalog_entry(&self, name: &str) -> Result<Option<CatalogEntry>> {
        self.run("catalog_entry", |c| {
            let st = c.stmt(Q::CatalogEntryNamed)?;
            let Some(r) = c.client.query_opt(&st, &[&name]).db()? else { return Ok(None) };
            entry_from_columns(
                &r.try_get::<_, String>(0).db()?,
                r.try_get(1).db()?,
                r.try_get(2).db()?,
                r.try_get(3).db()?,
                r.try_get(4).db()?,
                &r.try_get::<_, String>(5).db()?,
                &r.try_get::<_, String>(6).db()?,
                &r.try_get::<_, String>(7).db()?,
                r.try_get(8).db()?,
            )
            .map(Some)
        })
    }

    fn catalog_entry_ext(&self, kind: CatalogKind, name: &str) -> Result<Option<CatalogEntryExt>> {
        self.run("catalog_entry_ext", |c| {
            let Some(r) = c.client.query_opt(CATALOG_EXT_SELECT_BY_KIND_NAME, &[&kind.as_str(), &name]).db()? else {
                return Ok(None);
            };
            ext_from_row(&r).map(Some)
        })
    }

    fn catalog_entries_with_facets(&self) -> Result<Vec<FacetedEntry>> {
        self.run("catalog_entries_with_facets", |c| {
            let mut out = Vec::new();
            for r in c
                .client
                .query(
                    "SELECT e.kind, e.name, e.hash, e.facets,
                        CASE WHEN e.kind = 'class'
                             THEN (SELECT min(b.tx) FROM class_bindings b WHERE b.class_name = e.name)
                             ELSE (SELECT min(b.tx) FROM name_bindings b WHERE b.fn_name = e.name) END,
                        e.source_name
                   FROM catalog_entries e
                  WHERE e.facets::jsonb <> '{}'::jsonb
                  ORDER BY e.kind COLLATE \"C\", e.name COLLATE \"C\"",
                    &[],
                )
                .db()?
            {
                let kind = CatalogKind::parse(&r.try_get::<_, String>(0).db()?)
                    .ok_or_else(|| invalid("catalog", "a stored entry has a bad kind"))?;
                out.push(FacetedEntry {
                    kind,
                    name: r.try_get(1).db()?,
                    hash: r.try_get(2).db()?,
                    facets: parse_facets(&r.try_get::<_, String>(3).db()?)?,
                    first_bound: r.try_get(4).db()?,
                    source_name: r.try_get(5).db()?,
                });
            }
            Ok(out)
        })
    }

    fn extensions_of(&self, class: &str) -> Result<Vec<CatalogEntryExt>> {
        check_function_name(class, "class")?;
        self.run("extensions_of", |c| {
            let mut out = Vec::new();
            for r in c
                .client
                .query(
                    "SELECT kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name
                   FROM catalog_entries WHERE extends = $1 ORDER BY name COLLATE \"C\"",
                    &[&class],
                )
                .db()?
            {
                out.push(ext_from_row(&r)?);
            }
            Ok(out)
        })
    }

    fn extensions_of_limit(&self, class: &str, limit: usize) -> Result<Vec<CatalogEntryExt>> {
        check_function_name(class, "class")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.run("extensions_of", |c| {
            let mut out = Vec::new();
            for r in c
                .client
                .query(
                    "SELECT kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name
                   FROM catalog_entries WHERE extends = $1 ORDER BY name COLLATE \"C\" LIMIT $2",
                    &[&class, &limit],
                )
                .db()?
            {
                out.push(ext_from_row(&r)?);
            }
            Ok(out)
        })
    }

    fn catalog_summary(&self) -> Result<Summary> {
        self.run("catalog_summary", |c| {
            let mut entries = Vec::new();
            for r in c.client.query(
                "SELECT e.kind, e.name, e.label,
                        CASE WHEN e.kind = 'function' THEN (SELECT COALESCE(sum(x.n), 0)::bigint FROM cell_counts x WHERE x.fn_hash = e.hash) ELSE 0 END
                   FROM catalog_entries e ORDER BY e.kind COLLATE \"C\", e.name COLLATE \"C\"",
                &[],
            ).db()? {
                let kind = CatalogKind::parse(&r.try_get::<_, String>(0).db()?)
                    .ok_or_else(|| invalid("catalog", "a stored entry has a bad kind"))?;
                entries.push(SummaryEntry { kind, name: r.try_get(1).db()?, label: r.try_get(2).db()?, n_cells: r.try_get(3).db()? });
            }
            let mut dictionaries = Vec::new();
            let groups = c.client.query(
                "SELECT owner, key, count(*) FROM key_values GROUP BY owner, key ORDER BY owner COLLATE \"C\", key COLLATE \"C\"",
                &[],
            ).db()?;
            for g in groups {
                let (owner, key): (String, String) = (g.try_get(0).db()?, g.try_get(1).db()?);
                let mut values = Vec::new();
                for r in c.client.query(
                    "SELECT value, label, n_cells FROM key_values WHERE owner = $1 AND key = $2
                      ORDER BY value COLLATE \"C\" LIMIT $3",
                    &[&owner, &key, &as_i64(SUMMARY_VALUES)],
                ).db()? {
                    values.push(DictionaryValue { value: r.try_get(0).db()?, label: r.try_get(1).db()?, n_cells: r.try_get(2).db()? });
                }
                dictionaries.push(Dictionary { owner, key, n_values: g.try_get(2).db()?, values });
            }
            Ok(Summary { entries, dictionaries })
        })
    }
}

impl PgStore {
    /// Writes catalogue rows, all or nothing, keyed by (kind, name) (the call's bounds are
    /// checked by the caller). An operator's row never replaces a publisher's. An existing row
    /// keeps its facets and `extends` unless `replace_ext` (the facets call), in which case
    /// they are replaced. A new text drops the embedding.
    fn write_catalog(&self, rows: &[CatalogRow<'_>], replace_ext: bool) -> Result<Vec<i64>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let replace = if replace_ext {
            ", facets = EXCLUDED.facets, extends = EXCLUDED.extends, source_name = EXCLUDED.source_name"
        } else {
            ""
        };
        let upsert = format!(
            "INSERT INTO catalog_entries (kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, now())
             ON CONFLICT (kind, name) DO UPDATE SET hash = EXCLUDED.hash, label = EXCLUDED.label,
                 description = EXCLUDED.description, args = EXCLUDED.args, examples = EXCLUDED.examples,
                 source = EXCLUDED.source, signed_by = EXCLUDED.signed_by, updated_at = now(){replace}
             WHERE catalog_entries.source = 'operator' OR EXCLUDED.source = 'publisher'
             RETURNING id"
        );
        self.run("upsert_catalog", |c| {
            let mut t = c.client.transaction().db()?;
            let mut ids = Vec::with_capacity(rows.len());
            for row in rows {
                let e = row.entry;
                let kind = e.kind.as_str();
                let args = args_json(&e.args);
                let examples = canonical_examples(&e.examples)?;
                let before = t
                    .query_opt(
                        "SELECT label, description FROM catalog_entries WHERE kind = $1 AND name = $2",
                        &[&kind, &e.name],
                    )
                    .db()?
                    .map(|r| -> Result<(String, String)> { Ok((r.try_get(0).db()?, r.try_get(1).db()?)) })
                    .transpose()?;
                let updated = t
                    .query_opt(
                        &upsert,
                        &[
                            &kind,
                            &e.name,
                            &e.hash,
                            &e.label,
                            &e.description,
                            &args,
                            &examples,
                            &e.source.as_str(),
                            &e.signed_by,
                            &row.facets,
                            &row.extends,
                            &row.source_name,
                        ],
                    )
                    .db()?;
                let id: i64 = match updated {
                    Some(r) => {
                        let id: i64 = r.try_get(0).db()?;
                        // A new text drops the embedding (the old one is of other words).
                        if let Some((label, description)) = before {
                            if self.vectors && entry_text(&label, &description) != entry_text(&e.label, &e.description)
                            {
                                t.execute("DELETE FROM embeddings WHERE ref_kind = 'entry' AND ref_id = $1", &[&id])
                                    .db()?;
                            }
                        }
                        id
                    }
                    // An operator's entry over a publisher's: kept as it is.
                    None => t
                        .query_one("SELECT id FROM catalog_entries WHERE kind = $1 AND name = $2", &[&kind, &e.name])
                        .db()?
                        .try_get(0)
                        .db()?,
                };
                ids.push(id);
            }
            t.commit().db()?;
            Ok(ids)
        })
    }
}

impl Pattern {
    /// The members as of `asof`, in key order.
    fn members(&self, c: &mut Conn, asof: &str) -> Result<Vec<SliceMember>> {
        let qs = [Q::SliceMembersAll, Q::SliceMembersText, Q::SliceMembersNum];
        self.rows(c, asof, qs, 4, member_row, |m| &m.version.cell_id)
    }

    /// The members' values as of `asof`, in key order.
    fn values(&self, c: &mut Conn, asof: &str) -> Result<Vec<SliceValue>> {
        let qs = [Q::SliceValuesAll, Q::SliceValuesText, Q::SliceValuesNum];
        self.rows(c, asof, qs, 10, value_row, |m| &m.version.cell_id)
    }

    /// The members' values as of `asof`, in key order, at most `limit`: the statement of
    /// `values` with the key order and the LIMIT in SQL. The key order of `cmp_keys` is, per
    /// key argument by name, the type's name, then the number (exact) or the text.
    fn values_limited(&self, c: &mut Conn, asof: &str, limit: i64) -> Result<Vec<SliceValue>> {
        let (drive, _) = self.drive(c, false)?;
        let [names, kinds, types, los, his] = self.columns();
        let keys: Vec<String> = self.args.iter().filter(|a| a.kind != Kind::Value).map(|a| a.name.clone()).collect();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        let (base, d_name, d_ty, d_lo, d_hi) = match drive {
            None => (Q::SliceValuesAll, "", "", "", ""),
            Some(a) => {
                let q = if a.ty == "number" { Q::SliceValuesNum } else { Q::SliceValuesText };
                (q, a.name.as_str(), a.ty.as_str(), a.lo.as_str(), a.hi.as_str())
            }
        };
        // $16.. name each key argument; its order is the type, then the number, then the text.
        let mut order = Vec::with_capacity(keys.len() * 3 + 1);
        for i in 0..keys.len() {
            let p = 16 + i;
            let of =
                |col: &str| format!("(SELECT a.{col} FROM cell_args a WHERE a.cell_id = c.cell_id AND a.name = ${p})");
            order.push(format!("{} COLLATE \"C\"", of("type")));
            order.push(of("num"));
            order.push(format!("{} COLLATE \"C\"", of("value")));
        }
        order.push("c.cell_id".to_string());
        let sql = format!("{} ORDER BY {} LIMIT ${}", base.sql(), order.join(", "), 16 + keys.len());
        // The parameters the base statement does not use (the drive's, when there is none) are
        // typed as its statement types them: the text of the statement alone would not say.
        let mut param_types = base.types().to_vec();
        param_types.extend(std::iter::repeat_n(postgres::types::Type::TEXT, keys.len()));
        param_types.push(postgres::types::Type::INT8);
        let st = c.client.prepare_typed(sql.as_str(), &param_types).db()?;
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
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
        ];
        for k in &keys {
            params.push(k);
        }
        params.push(&limit);
        let rows = c.client.query(&st, &params).db()?;
        rows.iter()
            .map(|r| {
                let cell_id: String = r.try_get(10).db()?;
                value_row(r, &cell_id)
            })
            .collect()
    }

    /// A page of the members' values as of `asof` in sort-key order, after `after` (exclusive),
    /// at most `limit` (CONTRACT-0.5.1 S2): the statement of `values` with the key order and the
    /// LIMIT in SQL. Its candidates are the function's cells in the index on (fn_hash, ns, fn,
    /// sort_key), so withdrawn and absent members are filtered before the LIMIT counts.
    fn values_by_key(&self, c: &mut Conn, asof: &str, after: Option<&[u8]>, limit: i64) -> Result<Vec<SliceValue>> {
        let [names, kinds, types, los, his] = self.columns();
        let keys: Vec<String> = self.args.iter().filter(|a| a.kind != Kind::Value).map(|a| a.name.clone()).collect();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        let (after_sql, limit_at) = if after.is_some() { (" AND c.sort_key > $16", 17) } else { ("", 16) };
        let sql = format!("{}{} ORDER BY c.sort_key LIMIT ${limit_at}", Q::SliceValuesAll.sql(), after_sql);
        let mut param_types = Q::SliceValuesAll.types().to_vec();
        if after.is_some() {
            param_types.push(postgres::types::Type::BYTEA);
        }
        param_types.push(postgres::types::Type::INT8);
        let st = c.client.prepare_typed(sql.as_str(), &param_types).db()?;
        // The drive's parameters are unused here (the statement reads every cell of the function).
        let (d_name, d_ty, d_lo, d_hi) = ("", "", "", "");
        let after_bytes: &[u8] = after.unwrap_or(&[]);
        let mut params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
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
        ];
        if after.is_some() {
            params.push(&after_bytes);
        }
        params.push(&limit);
        let rows = c.client.query(&st, &params).db()?;
        rows.iter()
            .map(|r| {
                let cell_id: String = r.try_get(10).db()?;
                value_row(r, &cell_id)
            })
            .collect()
    }

    /// The members' values as of `asof` in cell id order, after `after` (exclusive), at most
    /// `limit`: the statement of `values` with the cell id order and the LIMIT in SQL (0.5.3).
    fn values_page(&self, c: &mut Conn, asof: &str, after: Option<&str>, limit: i64) -> Result<Vec<SliceValue>> {
        let (drive, _) = self.drive(c, false)?;
        let [names, kinds, types, los, his] = self.columns();
        let keys: Vec<String> = self.args.iter().filter(|a| a.kind != Kind::Value).map(|a| a.name.clone()).collect();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        let (base, d_name, d_ty, d_lo, d_hi) = match drive {
            None => (Q::SliceValuesAll, "", "", "", ""),
            Some(a) => {
                let q = if a.ty == "number" { Q::SliceValuesNum } else { Q::SliceValuesText };
                (q, a.name.as_str(), a.ty.as_str(), a.lo.as_str(), a.hi.as_str())
            }
        };
        let sql = format!("{} AND ($16::text IS NULL OR c.cell_id > $16) ORDER BY c.cell_id LIMIT $17", base.sql());
        let mut param_types = base.types().to_vec();
        param_types.push(postgres::types::Type::TEXT);
        param_types.push(postgres::types::Type::INT8);
        let st = c.client.prepare_typed(sql.as_str(), &param_types).db()?;
        let params: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![
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
            &after,
            &limit,
        ];
        let rows = c.client.query(&st, &params).db()?;
        rows.iter()
            .map(|r| {
                let cell_id: String = r.try_get(10).db()?;
                value_row(r, &cell_id)
            })
            .collect()
    }

    /// The cells with a computed version answering as of `asof`, in cell id order, at most
    /// `limit` (`computed_cells`); the candidates narrowed as the member queries narrow them.
    fn computed_cells(&self, c: &mut Conn, asof: &str, limit: i64) -> Result<Vec<Cell>> {
        let (drive, _) = self.drive(c, false)?;
        let [names, kinds, types, los, his] = self.columns();
        let keys: Vec<String> = self.args.iter().filter(|a| a.kind != Kind::Value).map(|a| a.name.clone()).collect();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        let (q, d_name, d_ty, d_lo, d_hi) = match drive {
            None => (Q::ComputedCellsAll, "", "", "", ""),
            Some(a) => {
                let q = if a.ty == "number" { Q::ComputedCellsNum } else { Q::ComputedCellsText };
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
                    &limit,
                ],
            )
            .db()?;
        rows.iter().map(cell_row).collect()
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

    /// Whether a matching cell has a stored version answering from after `asof`: from
    /// whichever side is smaller (both probed, up to 10,001), the function's receipts with an
    /// asof after `asof` (then each one's cell checked against the slice), or the slice's
    /// candidate cells (then each one's receipts by cell and asof).
    fn changed_after(&self, c: &mut Conn, asof: &str) -> Result<bool> {
        let (drive, candidates) = self.drive(c, true)?;
        let st = c.stmt(Q::ProbeRecent)?;
        let recent: i64 = c.client.query_one(&st, &[&self.fn_hash, &asof]).db()?.try_get(0).db()?;
        let qs = if recent <= candidates.unwrap_or(i64::MAX) {
            [Q::ChangedRecent; 3]
        } else {
            [Q::ChangedCandAll, Q::ChangedCandText, Q::ChangedCandNum]
        };
        let rows = self.query(c, asof, qs, drive)?;
        match rows.first() {
            Some(r) => r.try_get(0).db(),
            None => Ok(false),
        }
    }

    /// The latest withdrawal among the members as of `asof`: the (tx, asof) of the greatest,
    /// one indexed query (0.6.0; SPEC 8.2).
    fn withdrawn_latest(&self, c: &mut Conn, asof: &str) -> Result<Option<(String, String)>> {
        let (drive, _) = self.drive(c, false)?;
        let qs = [Q::SliceWithdrawnAll, Q::SliceWithdrawnText, Q::SliceWithdrawnNum];
        let rows = self.query(c, asof, qs, drive)?;
        match rows.first() {
            Some(r) => Ok(Some((r.try_get(0).db()?, r.try_get(1).db()?))),
            None => Ok(None),
        }
    }

    /// The bound or ranged argument holding the fewest cells (probed, up to 10,001, when
    /// there are several, or when `count` asks), else none (every cell of the function); and
    /// the number of candidates when probed.
    fn drive(&self, c: &mut Conn, count: bool) -> Result<(Option<&Constraint>, Option<i64>)> {
        let mut probe = |a: Option<&Constraint>| -> Result<i64> {
            match a {
                None => {
                    let st = c.stmt(Q::ProbeCells)?;
                    c.client.query_one(&st, &[&self.fn_hash]).db()?.try_get(0).db()
                }
                Some(a) => {
                    let st = c.stmt(if a.ty == "number" { Q::SliceProbeNum } else { Q::SliceProbeText })?;
                    c.client.query_one(&st, &[&self.fn_hash, &a.name, &a.ty, &a.lo, &a.hi]).db()?.try_get(0).db()
                }
            }
        };
        let constrained: Vec<&Constraint> = self.args.iter().filter(|a| a.kind != Kind::Open).collect();
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

    /// Runs one of a family of slice queries `qs` (all cells, a text range, a number range)
    /// with the slice's parameters and the candidates narrowed by `drive`.
    fn query(&self, c: &mut Conn, asof: &str, qs: [Q; 3], drive: Option<&Constraint>) -> Result<Vec<postgres::Row>> {
        let [names, kinds, types, los, his] = self.columns();
        let keys: Vec<String> = self.args.iter().filter(|a| a.kind != Kind::Value).map(|a| a.name.clone()).collect();
        let nargs = i32::try_from(self.args.len()).unwrap_or(i32::MAX);
        let (q, d_name, d_ty, d_lo, d_hi) = match drive {
            None => (qs[0], "", "", "", ""),
            Some(a) => {
                let q = if a.ty == "number" { qs[2] } else { qs[1] };
                (q, a.name.as_str(), a.ty.as_str(), a.lo.as_str(), a.hi.as_str())
            }
        };
        let st = c.stmt(q)?;
        c.client
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
        let (drive, _) = self.drive(c, false)?;
        let rows = self.query(c, asof, qs, drive)?;
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

/// Whether a word can be matched by the SQL prefilter exactly: JSON text escapes `"` and `\`
/// and control characters, and a word with them is matched in Rust only.
fn pg_safe(word: &str) -> bool {
    !word.chars().any(|c| c == '"' || c == '\\' || c.is_control())
}

/// The `AND (...)` prefilter of a text search over `fields` (SQL expressions of text): one
/// clause per word the SQL can match, each word a LIKE pattern over lower-cased fields, and
/// the parameters they take (numbered from `first`).
fn like_where(words: &Words, fields: &[&str], first: usize) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut params = Vec::new();
    for i in 0..words.len() {
        if let Some(p) = words.pattern(i, pg_safe) {
            params.push(p);
            let n = first + params.len() - 1;
            let ors: Vec<String> = fields.iter().map(|f| format!("lower({f}) LIKE ${n} ESCAPE '!'")).collect();
            sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
        }
    }
    (sql, params)
}

/// A function test row of `function_tests`, in the column order of `FunctionTestsOf`.
/// Adds to a function's counter row (0.5.3): `n` tests and the deltas of their results, and the
/// latest run of them. The row is made first, with the hash's branch counts (or `init`).
#[allow(clippy::too_many_arguments)]
fn bump_counter(
    t: &mut postgres::Transaction<'_>,
    ensure: &postgres::Statement,
    add: &postgres::Statement,
    fn_hash: &str,
    fn_name: &str,
    n: i64,
    results: [i64; 4],
    last_run_at: Option<&str>,
    init: (i32, i32),
) -> Result<()> {
    let (total, covered) = init;
    t.execute(ensure, &[&fn_hash, &fn_name, &total, &covered]).db()?;
    let [pass, fail, error, not_run] = results;
    let last_run_at = last_run_at.map(str::to_string);
    t.execute(add, &[&fn_hash, &fn_name, &n, &pass, &fail, &error, &not_run, &last_run_at]).db()?;
    Ok(())
}

fn function_test_row(r: &postgres::Row) -> Result<FunctionTest> {
    // The branch counts are `integer` with a check of at least zero: an `as` is exact.
    let total: i32 = r.try_get(7).db()?;
    let covered: i32 = r.try_get(8).db()?;
    Ok(FunctionTest {
        fn_hash: r.try_get(0).db()?,
        case: r.try_get(1).db()?,
        test_json: r.try_get(2).db()?,
        signed_by: r.try_get(3).db()?,
        last_run_at: r.try_get(4).db()?,
        last_result: r.try_get(5).db()?,
        last_detail: r.try_get(6).db()?,
        branches_total: total as u32,
        branches_covered: covered as u32,
    })
}

/// The columns of a catalogue entry with its facets and `extends`, by kind and name.
const CATALOG_EXT_SELECT_BY_KIND_NAME: &str =
    "SELECT kind, name, hash, label, description, args, examples, source, signed_by, facets, extends, source_name
       FROM catalog_entries WHERE kind = $1 AND name = $2";

/// A catalogue entry with its facets and `extends` from a row of [`CATALOG_EXT_SELECT_BY_KIND_NAME`]
/// (or of any query with those eleven columns in that order).
fn ext_from_row(r: &postgres::Row) -> Result<CatalogEntryExt> {
    let entry = entry_from_columns(
        &r.try_get::<_, String>(0).db()?,
        r.try_get(1).db()?,
        r.try_get(2).db()?,
        r.try_get(3).db()?,
        r.try_get(4).db()?,
        &r.try_get::<_, String>(5).db()?,
        &r.try_get::<_, String>(6).db()?,
        &r.try_get::<_, String>(7).db()?,
        r.try_get(8).db()?,
    )?;
    entry_ext(entry, &r.try_get::<_, String>(9).db()?, r.try_get(10).db()?, r.try_get(11).db()?)
}
