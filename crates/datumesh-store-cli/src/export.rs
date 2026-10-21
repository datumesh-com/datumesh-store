//! `export`: everything as JSON lines, in an order `import` can replay: namespaces (the
//! receipts are verified against them), descriptors, receipts, blobs, versions (per cell in
//! tx order), name bindings (per name in tx order). One consistent snapshot (a read-only
//! REPEATABLE READ transaction), streamed.
//!
//! ```text
//! {"namespace":{"host":"…","keydoc":"<key document JSON text>","validated_at":"…"}}
//! {"descriptor":"<descriptor JSON text>"}
//! {"receipt":"<receipt JSON text>"}
//! {"blob":"<content, lowercase hex>"}
//! {"version":{"cell_id":"…","receipt_hash":"…","tx":"…"}}
//! {"name":{"fn_hash":"…","fn_name":"…","tx":"…"}}
//! ```

use std::io::{BufWriter, Write};

use datumesh_core::json::Value;
use datumesh_core::lexical::to_hex;
use datumesh_store::postgres::fallible_iterator::FallibleIterator;
use datumesh_store::postgres::types::ToSql;
use datumesh_store::postgres::Transaction;

use crate::{connect, Fail};

fn s(x: impl Into<String>) -> Value {
    Value::String(x.into())
}

fn obj(members: Vec<(&str, Value)>) -> Value {
    Value::Object(members.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn line(out: &mut impl Write, kind: &str, v: Value) -> Result<(), Fail> {
    out.write_all(&obj(vec![(kind, v)]).to_jcs())?;
    out.write_all(b"\n")?;
    Ok(())
}

fn each(
    t: &mut Transaction<'_>,
    sql: &str,
    mut f: impl FnMut(&datumesh_store::postgres::Row) -> Result<(), Fail>,
) -> Result<(), Fail> {
    let params: [&(dyn ToSql + Sync); 0] = [];
    let mut rows = t.query_raw(sql, params)?;
    while let Some(r) = rows.next()? {
        f(&r)?;
    }
    Ok(())
}

fn text(b: Vec<u8>) -> Result<String, Fail> {
    String::from_utf8(b).map_err(|e| Fail(1, format!("stored JSON is not UTF-8: {e}")))
}

pub(crate) fn run(url: &str) -> Result<u8, Fail> {
    let mut c = connect(url)?;
    let mut t = c
        .build_transaction()
        .isolation_level(datumesh_store::postgres::IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()?;
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    each(&mut t, "SELECT host, keydoc, validated_at FROM namespaces ORDER BY host", |r| {
        let (host, doc, at): (String, Vec<u8>, String) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
        line(&mut out, "namespace", obj(vec![("host", s(host)), ("keydoc", s(text(doc)?)), ("validated_at", s(at))]))
    })?;
    each(&mut t, "SELECT bytes FROM descriptors ORDER BY fn_hash", |r| {
        line(&mut out, "descriptor", s(text(r.try_get(0)?)?))
    })?;
    each(&mut t, "SELECT bytes FROM receipts ORDER BY hash", |r| line(&mut out, "receipt", s(text(r.try_get(0)?)?)))?;
    each(&mut t, "SELECT bytes FROM blobs ORDER BY sha256", |r| {
        let b: Vec<u8> = r.try_get(0)?;
        line(&mut out, "blob", s(to_hex(&b)))
    })?;
    each(&mut t, "SELECT cell_id, tx, receipt_hash FROM versions ORDER BY cell_id, tx", |r| {
        let (cell, tx, h): (String, String, String) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
        line(&mut out, "version", obj(vec![("cell_id", s(cell)), ("tx", s(tx)), ("receipt_hash", s(h))]))
    })?;
    each(&mut t, "SELECT fn_name, tx, fn_hash FROM name_bindings ORDER BY fn_name, tx", |r| {
        let (name, tx, h): (String, String, String) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
        line(&mut out, "name", obj(vec![("fn_name", s(name)), ("tx", s(tx)), ("fn_hash", s(h))]))
    })?;
    out.flush()?;
    t.commit()?;
    Ok(0)
}
