//! `export`: everything as JSON lines, in an order `import` can replay: namespaces (the
//! receipts are verified against them), descriptors, receipts, manifests (by entry; since
//! 0.3.1), blobs, versions (per cell in tx order), computed versions (per cell, by tx and
//! asof; since 0.2.0), name bindings (per name in tx order). The slice-entry lineage and the
//! typed-argument index are not lines of their own: `put_receipt` derives them from the
//! receipts. One consistent snapshot (PostgreSQL: a read-only REPEATABLE READ transaction;
//! SQLite: one read transaction, a snapshot in WAL mode), streamed. The same lines from
//! either backend for the same contents.
//! `import` reads 0.1.x, 0.2.x and 0.3.0 exports too (they have no manifests, and 0.1.x no
//! computed versions).
//!
//! ```text
//! {"namespace":{"host":"…","keydoc":"<key document JSON text>","validated_at":"…"}}
//! {"descriptor":"<descriptor JSON text>"}
//! {"receipt":"<receipt JSON text>"}
//! {"manifest":"<manifest JSON text>"}
//! {"blob":"<content, lowercase hex>"}
//! {"version":{"cell_id":"…","receipt_hash":"…","tx":"…"}}
//! {"computed":{"asof":"…","cell_id":"…","receipt_hash":"…","tx":"…"}}
//! {"name":{"fn_hash":"…","fn_name":"…","tx":"…"}}
//! ```

use std::io::{BufWriter, Write};

use datumesh_core::json::Value;
use datumesh_core::lexical::to_hex;
use postgres::fallible_iterator::FallibleIterator;
use postgres::types::ToSql;

use crate::{connect, Fail};

/// A column of an exported row: text, or bytes.
enum Field {
    Text(String),
    Bytes(Vec<u8>),
}

impl Field {
    fn text(&self) -> Result<String, Fail> {
        match self {
            Field::Text(t) => Ok(t.clone()),
            Field::Bytes(b) => {
                String::from_utf8(b.clone()).map_err(|e| Fail(1, format!("stored JSON is not UTF-8: {e}")))
            }
        }
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Field::Text(t) => t.clone().into_bytes(),
            Field::Bytes(b) => b.clone(),
        }
    }
}

/// A snapshot of a database, read query by query: each row's columns given to `f`.
trait Snapshot {
    fn each(&mut self, sql: &str, bytes: &[usize], f: &mut dyn FnMut(&[Field]) -> Result<(), Fail>)
        -> Result<(), Fail>;
}

struct PgSnapshot<'a>(postgres::Transaction<'a>);

impl Snapshot for PgSnapshot<'_> {
    fn each(
        &mut self,
        sql: &str,
        bytes: &[usize],
        f: &mut dyn FnMut(&[Field]) -> Result<(), Fail>,
    ) -> Result<(), Fail> {
        let params: [&(dyn ToSql + Sync); 0] = [];
        let mut rows = self.0.query_raw(sql, params)?;
        while let Some(r) = rows.next()? {
            let fields = (0..r.len())
                .map(|i| Ok(if bytes.contains(&i) { Field::Bytes(r.try_get(i)?) } else { Field::Text(r.try_get(i)?) }))
                .collect::<Result<Vec<Field>, postgres::Error>>()?;
            f(&fields)?;
        }
        Ok(())
    }
}

struct LiteSnapshot(rusqlite::Connection);

impl Snapshot for LiteSnapshot {
    fn each(
        &mut self,
        sql: &str,
        bytes: &[usize],
        f: &mut dyn FnMut(&[Field]) -> Result<(), Fail>,
    ) -> Result<(), Fail> {
        let mut st = self.0.prepare(sql)?;
        let n = st.column_count();
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let fields = (0..n)
                .map(|i| Ok(if bytes.contains(&i) { Field::Bytes(r.get(i)?) } else { Field::Text(r.get(i)?) }))
                .collect::<Result<Vec<Field>, rusqlite::Error>>()?;
            f(&fields)?;
        }
        Ok(())
    }
}

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

pub(crate) fn run(url: &str) -> Result<u8, Fail> {
    if !crate::is_postgres(url) {
        let path = match crate::open(url)? {
            crate::Opened::Lite(s) => s.path().to_path_buf(),
            crate::Opened::Pg(_) => unreachable!("not a PostgreSQL URL"),
        };
        let c = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        c.busy_timeout(std::time::Duration::from_secs(30))?;
        // One read transaction: a snapshot (WAL) until the end.
        c.execute_batch("BEGIN")?;
        let mut snap = LiteSnapshot(c);
        write(&mut snap)?;
        snap.0.execute_batch("COMMIT")?;
        return Ok(0);
    }
    let mut c = connect(url)?;
    let t = c.build_transaction().isolation_level(postgres::IsolationLevel::RepeatableRead).read_only(true).start()?;
    let mut snap = PgSnapshot(t);
    write(&mut snap)?;
    snap.0.commit()?;
    Ok(0)
}

/// Every line, in import order.
fn write(t: &mut dyn Snapshot) -> Result<(), Fail> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    t.each("SELECT host, keydoc, validated_at FROM namespaces ORDER BY host", &[1], &mut |r| {
        line(
            &mut out,
            "namespace",
            obj(vec![("host", s(r[0].text()?)), ("keydoc", s(r[1].text()?)), ("validated_at", s(r[2].text()?))]),
        )
    })?;
    t.each("SELECT bytes FROM descriptors ORDER BY fn_hash", &[0], &mut |r| {
        line(&mut out, "descriptor", s(r[0].text()?))
    })?;
    t.each("SELECT bytes FROM receipts ORDER BY hash", &[0], &mut |r| line(&mut out, "receipt", s(r[0].text()?)))?;
    t.each("SELECT bytes FROM manifests ORDER BY entry", &[0], &mut |r| line(&mut out, "manifest", s(r[0].text()?)))?;
    t.each("SELECT bytes FROM blobs ORDER BY sha256", &[0], &mut |r| line(&mut out, "blob", s(to_hex(&r[0].bytes()))))?;
    t.each("SELECT cell_id, tx, receipt_hash FROM versions ORDER BY cell_id, tx", &[], &mut |r| {
        line(
            &mut out,
            "version",
            obj(vec![("cell_id", s(r[0].text()?)), ("tx", s(r[1].text()?)), ("receipt_hash", s(r[2].text()?))]),
        )
    })?;
    t.each(
        "SELECT cell_id, tx, asof, receipt_hash FROM computed_versions ORDER BY cell_id, tx, asof",
        &[],
        &mut |r| {
            line(
                &mut out,
                "computed",
                obj(vec![
                    ("cell_id", s(r[0].text()?)),
                    ("tx", s(r[1].text()?)),
                    ("asof", s(r[2].text()?)),
                    ("receipt_hash", s(r[3].text()?)),
                ]),
            )
        },
    )?;
    t.each("SELECT fn_name, tx, fn_hash FROM name_bindings ORDER BY fn_name, tx", &[], &mut |r| {
        line(
            &mut out,
            "name",
            obj(vec![("fn_name", s(r[0].text()?)), ("tx", s(r[1].text()?)), ("fn_hash", s(r[2].text()?))]),
        )
    })?;
    out.flush()?;
    Ok(())
}
