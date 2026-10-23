//! `import`: specification vector files (`*.json`, also inside directories) and JSON lines
//! (anything else, or `-` / nothing for stdin; the format `export` writes).
//!
//! From a vector it takes what a store holds: a valid function vector's descriptor; a valid
//! receipt vector's key documents (each 200 response with a body, as its host's namespace,
//! validated at the time of the import; first, since receipts are verified against them),
//! its receipt, the descriptor and input receipts of its `recompute` object, its blob
//! content, and, when the recompute descriptor is `stored`, the receipt as the cell's
//! version at its tx. Importing what is already there changes nothing.

use std::io::BufRead;
use std::path::Path;

use datumesh_core::json::{self, Value};
use datumesh_core::{Descriptor, Receipt};
use datumesh_store::{Error, PgStore, Resolver, Store};

use crate::Fail;

#[derive(Default)]
struct Counts {
    descriptors: u64,
    receipts: u64,
    blobs: u64,
    versions: u64,
    computed: u64,
    namespaces: u64,
    names: u64,
    errors: u64,
}

pub(crate) fn run(url: &str, paths: &[String]) -> Result<u8, Fail> {
    let store = crate::store(url)?;
    let mut n = Counts::default();
    if paths.is_empty() || paths == ["-"] {
        lines(&store, "stdin", std::io::stdin().lock(), &mut n);
    } else {
        for p in paths {
            if p == "-" {
                lines(&store, "stdin", std::io::stdin().lock(), &mut n);
            } else {
                path(&store, Path::new(p), &mut n)?;
            }
        }
    }
    println!(
        "imported {} descriptors, {} receipts, {} blobs, {} versions, {} computed versions, {} namespaces, {} names; {} errors",
        n.descriptors, n.receipts, n.blobs, n.versions, n.computed, n.namespaces, n.names, n.errors
    );
    Ok(if n.errors == 0 { 0 } else { 1 })
}

fn path(store: &PgStore, p: &Path, n: &mut Counts) -> Result<(), Fail> {
    if p.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(p)
            .map_err(|e| Fail(1, format!("{}: {e}", p.display())))?
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        for e in entries {
            if e.is_dir() || e.extension().is_some_and(|x| x == "json") {
                path(store, &e, n)?;
            }
        }
        return Ok(());
    }
    let name = p.display().to_string();
    if p.extension().is_some_and(|x| x == "json") {
        let bytes = std::fs::read(p).map_err(|e| Fail(1, format!("{name}: {e}")))?;
        match json::parse(&bytes) {
            Ok(v) => vector(store, &name, &v, n),
            Err(e) => report(n, &name, &format!("not JSON: {e}")),
        }
    } else {
        let f = std::fs::File::open(p).map_err(|e| Fail(1, format!("{name}: {e}")))?;
        lines(store, &name, std::io::BufReader::new(f), n);
    }
    Ok(())
}

fn report(n: &mut Counts, at: &str, msg: &str) {
    n.errors += 1;
    eprintln!("{at}: {msg}");
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn vector(store: &PgStore, name: &str, v: &Value, n: &mut Counts) {
    if !matches!(v.get("valid"), Some(Value::Bool(true))) {
        return;
    }
    let mut errors: Vec<Error> = Vec::new();
    let mut put = |r: Result<(), Error>| {
        if let Err(e) = r {
            errors.push(e);
        }
    };
    match str_of(v, "kind") {
        Some("function") => {
            if let Some(t) = str_of(v, "input") {
                put(descriptor(store, t.as_bytes(), &mut n.descriptors));
            }
        }
        Some("receipt") => {
            let Some(text) = str_of(v, "input") else { return };
            for (host, env) in v.get("keys").and_then(Value::as_object).unwrap_or(&[]) {
                let https = env.get("https");
                let status = https.and_then(|h| h.get("status")).and_then(Value::as_number_text);
                if let (Some("200"), Some(body)) = (status, https.and_then(|h| h.get("body")).and_then(Value::as_str)) {
                    put(namespace(store, host, body.as_bytes(), &mut n.namespaces));
                }
            }
            let mut stored = false;
            if let Some(rc) = v.get("recompute") {
                if let Some(d) = rc.get("descriptor") {
                    let bytes = d.to_jcs();
                    stored = Descriptor::from_json(&bytes).is_ok_and(|d| d.lang == "stored");
                    put(descriptor(store, &bytes, &mut n.descriptors));
                }
                for (_, r) in rc.get("inputs").and_then(Value::as_object).unwrap_or(&[]) {
                    if let Some(t) = r.as_str() {
                        put(receipt(store, t.as_bytes(), &mut n.receipts));
                    }
                }
            }
            put(receipt(store, text.as_bytes(), &mut n.receipts));
            if let Some(h) = str_of(v, "blob_hex") {
                match unhex(h) {
                    Some(b) => put(store.put_blob(&b).map(|_| n.blobs += 1)),
                    None => put(Err(Error::Migration("blob_hex is not hex".into()))),
                }
            }
            if stored {
                if let Ok(r) = Receipt::from_json(text.as_bytes()) {
                    put(version(store, &r.cell().id(), r.tx(), r.hash(), &mut n.versions));
                }
            }
        }
        _ => {}
    }
    for e in errors {
        report(n, name, &e.to_string());
    }
}

/// A vector's key document as the host's namespace, validated now, unless the store already
/// holds this very document (re-importing does not re-validate).
fn namespace(store: &PgStore, host: &str, body: &[u8], count: &mut u64) -> Result<(), Error> {
    let same = match (store.namespace(host), json::parse(body)) {
        (Ok(Some(ns)), Ok(doc)) => ns.keydoc == doc.to_jcs(),
        _ => false,
    };
    if !same {
        store.put_namespace(host, body, &now())?;
    }
    *count += 1;
    Ok(())
}

/// The current time as an instant.
fn now() -> String {
    let us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros());
    crate::bench::instant(i64::try_from(us).unwrap_or(i64::MAX))
}

fn descriptor(store: &PgStore, bytes: &[u8], count: &mut u64) -> Result<(), Error> {
    store.put_descriptor(bytes)?;
    *count += 1;
    Ok(())
}

fn receipt(store: &PgStore, bytes: &[u8], count: &mut u64) -> Result<(), Error> {
    store.put_receipt(bytes)?;
    *count += 1;
    Ok(())
}

/// put_version, where the same version already present counts as imported.
fn version(store: &PgStore, cell_id: &str, tx: &str, hash: &str, count: &mut u64) -> Result<(), Error> {
    match store.put_version(cell_id, tx, hash) {
        Err(Error::NotLater { .. })
            if store.version_asof(cell_id, tx)?.is_some_and(|v| v.tx == tx && v.receipt_hash == hash) => {}
        r => r?,
    }
    *count += 1;
    Ok(())
}

fn unhex(h: &str) -> Option<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| h.get(i..i + 2).and_then(|x| u8::from_str_radix(x, 16).ok())).collect()
}

fn lines(store: &PgStore, name: &str, input: impl BufRead, n: &mut Counts) {
    for (i, l) in input.split(b'\n').enumerate() {
        let at = format!("{name}: line {}", i + 1);
        let l = match l {
            Ok(l) => l,
            Err(e) => return report(n, &at, &e.to_string()),
        };
        if l.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Err(msg) = one_line(store, &l, n) {
            report(n, &at, &msg);
        }
    }
}

fn one_line(store: &PgStore, l: &[u8], n: &mut Counts) -> Result<(), String> {
    let v = json::parse(l).map_err(|e| format!("not JSON: {e}"))?;
    let members = v.as_object().filter(|m| m.len() == 1).ok_or("expected an object with one member")?;
    let (kind, body) = &members[0];
    let text = |k: &str| -> Result<String, String> {
        body.get(k).and_then(Value::as_str).map(str::to_string).ok_or(format!("{kind}: no string member {k:?}"))
    };
    let e = |e: Error| e.to_string();
    match kind.as_str() {
        "descriptor" => {
            let t = body.as_str().ok_or("descriptor: expected a string")?;
            descriptor(store, t.as_bytes(), &mut n.descriptors).map_err(e)
        }
        "receipt" => {
            let t = body.as_str().ok_or("receipt: expected a string")?;
            receipt(store, t.as_bytes(), &mut n.receipts).map_err(e)
        }
        "blob" => {
            let b = body.as_str().and_then(unhex).ok_or("blob: expected hex")?;
            store.put_blob(&b).map_err(e)?;
            n.blobs += 1;
            Ok(())
        }
        "version" => {
            version(store, &text("cell_id")?, &text("tx")?, &text("receipt_hash")?, &mut n.versions).map_err(e)
        }
        "computed" => {
            let (cell_id, tx, asof, hash) = (text("cell_id")?, text("tx")?, text("asof")?, text("receipt_hash")?);
            // The key's asof is the receipt's: a line naming another is refused.
            if let Some(bytes) = store.receipt(&hash) {
                let r = Receipt::from_json(&bytes).map_err(|e| format!("computed: stored receipt {hash}: {e}"))?;
                if r.asof() != asof {
                    return Err(format!("computed: asof {asof} is not receipt {hash}'s, {}", r.asof()));
                }
            }
            store.put_computed(&cell_id, &tx, &hash).map_err(e)?;
            n.computed += 1;
            Ok(())
        }
        "namespace" => {
            store.put_namespace(&text("host")?, text("keydoc")?.as_bytes(), &text("validated_at")?).map_err(e)?;
            n.namespaces += 1;
            Ok(())
        }
        "name" => {
            let (name, h, tx) = (text("fn_name")?, text("fn_hash")?, text("tx")?);
            match store.bind_name(&name, &h, &tx) {
                // Already bound to this hash at that time: imported before.
                Err(Error::NotLater { .. })
                    if store.resolve_name(&name, &tx).map_err(e)?.as_deref() == Some(h.as_str()) => {}
                r => r.map_err(e)?,
            }
            n.names += 1;
            Ok(())
        }
        other => Err(format!("unknown kind {other:?}")),
    }
}
