//! Classes in the store (0.4.1; SPEC 16.1, 16.2, 16.3, 9.7): `put_class` and `class` keep the
//! flattened class descriptor by its class hash; `bind_class`, `resolve_class` and
//! `resolve_class_binding` bind a class name over transaction time, binding each method
//! `N/m` to its function hash in the same transaction. Class names and function names are
//! one namespace of names. Writes under a suspended namespace are refused, reads never are.
//! The store records the owner (16.2 step 1) and does not enforce it. Both backends.
//!
//! The class descriptors are real ones, made with the core's `define_class`.
//! Every test here fails until the behaviour is implemented (the methods are `todo!()`
//! stubs; see `IMPL-NOTES-0.4.1-classes.md`).

#[macro_use]
mod common;

use std::collections::BTreeMap;

use common::*;
use datumesh_core::{ClassDescriptor, ClassEnv, ErrorCode, FunctionDescriptor};
use datumesh_store::{migrate, ClassBinding, Error, Store};

const HOST: &str = "rates.example.org";
const CLASS: &str = "rates.example.org/fx_pair";
const SUB: &str = "rates.example.org/fx_forward";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";

// ---- Real class descriptors, made with the core ------------------------------------------

/// A function descriptor (stored) of a class method: the keys `base` and `quote` (upper_ascii
/// strings), then `date`, returning a number; `tag` tells functions apart.
fn method_fn(tag: &str) -> Vec<u8> {
    let key = r#"{"key_normalisation":"upper_ascii","type":"string","valid":false}"#;
    jcs(format!(
        r#"{{"code":{{"body":"{tag}","lang":"stored"}},"inputs":[],"signature":{{"args":{{"base":{key},"date":{{"type":"date","valid":true}},"quote":{key}}},"order":["base","quote","date"],"returns":{{"type":"number","unit":"1"}}}}}}"#
    )
    .as_bytes())
}

/// A class made by `define_class`, with what the store needs to hold it.
struct Built {
    /// The flattened descriptor's canonical bytes.
    bytes: Vec<u8>,
    /// The class hash.
    hash: String,
    /// Each method (flattened, inherited ones too) with its function hash.
    methods: BTreeMap<String, String>,
    /// The descriptors of the method functions this class's author wrote (not the parent's).
    functions: Vec<Vec<u8>>,
    flat: ClassDescriptor,
}

/// The author's text of `fx_pair`: two upper_ascii keys, methods `rate` and `spot`. Members
/// are written out of canonical order and with white space, as an author writes them.
fn fx_text(steward: &str, methods: &[(&str, &str)]) -> String {
    let m: Vec<String> = methods.iter().map(|(n, h)| format!(r#""{n}": "{h}""#)).collect();
    format!(
        r#"{{
  "steward": {{"process": "https://{steward}/classes/change", "namespace": "{steward}"}},
  "order": ["base", "quote"],
  "methods": {{{}}},
  "keys": {{
    "quote": {{"type": "string", "key_normalisation": "upper_ascii"}},
    "base": {{"type": "string", "key_normalisation": "upper_ascii"}}
  }}
}}"#,
        m.join(", ")
    )
}

fn env_with(definer: &str, functions: &[&Vec<u8>], classes: &[&ClassDescriptor]) -> ClassEnv {
    let mut env = ClassEnv::new(definer);
    for f in functions {
        env.insert_function(FunctionDescriptor::from_json(f).unwrap());
    }
    for c in classes {
        env.insert_class((*c).clone());
    }
    env
}

fn built(flat: ClassDescriptor, functions: Vec<Vec<u8>>) -> Built {
    Built {
        bytes: flat.canonical(),
        hash: flat.hash(),
        methods: flat.methods.iter().map(|(m, h)| (m.clone(), h.clone())).collect(),
        functions,
        flat,
    }
}

/// `fx_pair` with methods `rate` (a function that depends on `version`) and `spot`.
fn fx(name: &str, steward: &str, version: &str) -> Built {
    let rate = method_fn(&format!("rate {version}"));
    let spot = method_fn("spot");
    let text = fx_text(steward, &[("rate", &fn_hash_of(&rate)), ("spot", &fn_hash_of(&spot))]);
    let definer = name.rsplit_once('/').unwrap().0;
    let flat = datumesh_core::define_class(name, text.as_bytes(), &env_with(definer, &[&rate, &spot], &[])).unwrap();
    built(flat, vec![rate, spot])
}

/// The default class: `fx_pair` under `HOST`, stewarded by it.
fn fx_pair() -> Built {
    fx(CLASS, HOST, "v1")
}

/// `fx_forward`, a subclass of `parent`: no key of its own, one method `forward` of its own,
/// so its flattened descriptor has `parent`, the parent's keys and methods, and `forward`.
fn fx_forward(parent: &Built) -> Built {
    let forward = method_fn("forward");
    let text = format!(
        r#"{{"keys":{{}},"methods":{{"forward":"{}"}},"order":[],"parent":"{}","steward":{{"namespace":"{HOST}","process":"https://{HOST}/classes/change"}}}}"#,
        fn_hash_of(&forward),
        parent.hash
    );
    let env = env_with(HOST, &[&forward], &[&parent.flat]);
    // The parent's methods are known to define as well.
    let mut env = env;
    for f in &parent.functions {
        env.insert_function(FunctionDescriptor::from_json(f).unwrap());
    }
    let flat = datumesh_core::define_class(SUB, text.as_bytes(), &env).unwrap();
    built(flat, vec![forward])
}

/// Stores every method function of `c` (those its author wrote).
fn put_functions(s: &(impl AsStore + ?Sized), c: &Built) {
    for f in &c.functions {
        s.st().put_descriptor(f).unwrap();
    }
}

fn method_name(class: &str, m: &str) -> String {
    format!("{class}/{m}")
}

fn bound_fn(s: &(impl AsStore + ?Sized), name: &str, asof: &str) -> Option<(String, String)> {
    s.st().resolve_binding(name, asof).unwrap().map(|b| (b.fn_hash, b.tx))
}

fn not_found(e: Error) -> (&'static str, String) {
    match e {
        Error::NotFound { what, key } => (what, key),
        e => panic!("not NotFound: {e:?}"),
    }
}

fn name_taken(e: Error) -> (String, &'static str) {
    match e {
        Error::NameTaken { name, bound_as } => (name, bound_as),
        e => panic!("not NameTaken: {e:?}"),
    }
}

fn suspended(e: Error) -> (String, String) {
    match e {
        Error::NamespaceSuspended { host, suspended_at } => (host, suspended_at),
        e => panic!("not NamespaceSuspended: {e:?}"),
    }
}

/// Suspends `host` at `at` (its key document first: a namespace is what is suspended).
fn suspend(s: &(impl AsStore + ?Sized), host: &str, at: &str) {
    let s = s.st();
    s.put_namespace(host, &key_doc(&vector("receipt/stored-number"), host), VALIDATED_AT).unwrap();
    assert!(s.record_namespace_failure(host, at, 1).unwrap().unwrap().suspended_at.is_some());
}

// ---- Item 1: class descriptors ------------------------------------------------------------

fn put_class_stores_the_flattened_descriptor_by_its_class_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    assert_eq!(s.class(&c.hash).unwrap(), None);
    // Neither the parent, nor the method functions, nor a key document need to be stored.
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    assert_eq!(s.class(&c.hash).unwrap(), Some(c.bytes.clone()));
    // The hash is the SHA-256 of the bytes stored, and the core reads them back as the
    // descriptor it flattened.
    let stored = s.class(&c.hash).unwrap().unwrap();
    assert_eq!(datumesh_core::sha256_hex(&stored), c.hash);
    assert_eq!(ClassDescriptor::from_json(&stored).unwrap(), c.flat);
    // Another class is another row.
    let other = fx(CLASS, HOST, "v2");
    assert_eq!(s.put_class(&other.bytes).unwrap(), other.hash);
    assert_eq!(s.class(&c.hash).unwrap(), Some(c.bytes.clone()));
    assert_eq!(s.class(&other.hash).unwrap(), Some(other.bytes.clone()));
    // Not stored: nothing; a malformed hash is the core's code.
    assert_eq!(s.class(&"0".repeat(64)).unwrap(), None);
    assert_eq!(s.class("ABC").unwrap_err().code(), Some(ErrorCode::CanonHex));
    assert_eq!(s.class(&c.hash.to_uppercase()).unwrap_err().code(), Some(ErrorCode::CanonHex));
}

fn put_class_is_idempotent_on_the_same_bytes(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    // From a store of its own on the same database too.
    assert_eq!(db.open_again().put_class(&c.bytes).unwrap(), c.hash);
    assert_eq!(s.class(&c.hash).unwrap(), Some(c.bytes.clone()));
}

fn put_class_stores_a_subclass_by_its_flattened_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let p = fx_pair();
    let c = fx_forward(&p);
    // The descriptors are the core's: flattened, with the inherited methods.
    assert_eq!(p.methods.keys().collect::<Vec<_>>(), ["rate", "spot"]);
    assert_eq!(c.methods.keys().collect::<Vec<_>>(), ["forward", "rate", "spot"]);
    assert_eq!(c.methods["rate"], p.methods["rate"]);
    assert_ne!(c.hash, p.hash);
    // The class name is not part of the hash; the steward and the methods are.
    assert_eq!(fx("rates.example.org/fx_copy", HOST, "v1").hash, p.hash);
    assert_ne!(fx(CLASS, HOST, "v2").hash, p.hash);
    // The parent need not be stored first (the definer had it; the store records).
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    let stored = ClassDescriptor::from_json(&s.class(&c.hash).unwrap().unwrap()).unwrap();
    assert_eq!(stored.parent.as_deref(), Some(p.hash.as_str()));
    assert_eq!(stored.methods, c.flat.methods);
    assert_eq!(stored.keys, p.flat.keys);
    assert_eq!(s.class(&p.hash).unwrap(), None);
    assert_eq!(s.put_class(&p.bytes).unwrap(), p.hash);
}

fn put_class_rejects_bytes_that_are_not_canonical(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    // The author's text: a valid descriptor (the core accepts it), members out of order and
    // white space. Its canonical form is `c.bytes`; the text itself is refused, not
    // canonicalised (as put_descriptor would).
    let text = fx_text(HOST, &[("rate", &c.methods["rate"]), ("spot", &c.methods["spot"])]);
    assert!(ClassDescriptor::from_json(text.as_bytes()).is_ok());
    assert_ne!(text.as_bytes(), &c.bytes[..]);
    assert_eq!(s.put_class(text.as_bytes()).unwrap_err().code(), Some(ErrorCode::Schema));
    // White space alone, and a trailing newline.
    let spaced = String::from_utf8(c.bytes.clone()).unwrap().replace(',', ", ");
    assert_eq!(s.put_class(spaced.as_bytes()).unwrap_err().code(), Some(ErrorCode::Schema));
    let mut newline = c.bytes.clone();
    newline.push(b'\n');
    assert_eq!(s.put_class(&newline).unwrap_err().code(), Some(ErrorCode::Schema));
    // Nothing was stored, under either hash.
    assert_eq!(s.class(&c.hash).unwrap(), None);
    assert_eq!(s.class(&datumesh_core::sha256_hex(text.as_bytes())).unwrap(), None);
    // The canonical bytes go in.
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
}

fn put_class_rejects_what_the_core_rejects(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    let canonical = String::from_utf8(c.bytes.clone()).unwrap();
    let cases: Vec<(&str, Vec<u8>, ErrorCode)> = vec![
        ("not JSON", b"{".to_vec(), ErrorCode::ParseJson),
        ("empty", Vec::new(), ErrorCode::ParseJson),
        ("an empty object: members missing", b"{}".to_vec(), ErrorCode::Schema),
        ("an unknown member", canonical.replace(r#"{"keys""#, r#"{"extra":1,"keys""#).into_bytes(), ErrorCode::Schema),
        // `order` must hold every key exactly once.
        (
            "order misses a key",
            canonical.replace(r#""order":["base","quote"]"#, r#""order":["base"]"#).into_bytes(),
            ErrorCode::CanonSet,
        ),
        // A method hash is 64 lowercase hex digits.
        ("a method hash not hex", canonical.replace(&c.methods["rate"], "XYZ").into_bytes(), ErrorCode::CanonHex),
    ];
    for (what, bytes, code) in cases {
        let e = s.put_class(&bytes).unwrap_err();
        assert_eq!(e.code(), Some(code), "{what}: {e}");
    }
    // The class itself was never stored by the failures.
    assert_eq!(s.class(&c.hash).unwrap(), None);
}

fn class_hashes_and_function_hashes_are_separate_rows(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();
    // A function hash is not a class, and a class hash is not a function.
    assert_eq!(s.class(&c.methods["rate"]).unwrap(), None);
    assert!(s.descriptor(&c.hash).is_none());
    assert!(s.descriptor(&c.methods["rate"]).is_some());
}

/// The migration is additive: a database at the 0.4.0/0.4.1 schema (PostgreSQL migrations 1-5,
/// SQLite 1-2) keeps its descriptors and function bindings, applies exactly one more
/// migration, and then holds classes.
fn the_class_migration_is_additive_and_keeps_function_bindings(b: Backend) {
    let db = TestDb::empty(b);
    let (old, all): (Vec<&migrate::Migration>, usize) = match b {
        Backend::Pg => (migrate::MIGRATIONS[..5].iter().collect(), migrate::MIGRATIONS.len()),
        Backend::Sqlite => (migrate::sqlite::MIGRATIONS[..2].iter().collect(), migrate::sqlite::MIGRATIONS.len()),
    };
    // Since 0.4.2 the audit column (CONTRACT-1.8b S3) is one more migration after classes.
    assert_eq!(all, old.len() + 2, "one migration for classes and one for the audit column, on each backend");
    db.exec(db.pick(
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))) STRICT",
    ))
    .unwrap();
    for m in &old {
        db.exec(m.sql).unwrap();
        db.exec(&format!(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ({}, '{}', '{}')",
            m.version,
            m.name,
            m.checksum()
        ))
        .unwrap();
    }
    // A function and its binding, written as 0.4.1 wrote them.
    let f = fx_pair().functions[0].clone();
    let h = fn_hash_of(&f);
    let hex: String = f.iter().map(|x| format!("{x:02x}")).collect();
    db.exec(db.pick(
        &format!("INSERT INTO descriptors (fn_hash, bytes) VALUES ('{h}', decode('{hex}', 'hex'))"),
        &format!("INSERT INTO descriptors (fn_hash, bytes) VALUES ('{h}', X'{hex}')"),
    ))
    .unwrap();
    db.exec(db.pick(
        &format!("INSERT INTO name_bindings (fn_name, tx, tx_at, fn_hash) VALUES ('{HOST}/rate', '{T1}', '{T1}'::timestamptz, '{h}')"),
        &format!("INSERT INTO name_bindings (fn_name, tx, fn_hash) VALUES ('{HOST}/rate', '{T1}', '{h}')"),
    ))
    .unwrap();

    let s = db.bare_store();
    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(datumesh_store::SqliteOptions::new())).unwrap(),
    };
    assert_eq!(applied, [all as u32 - 1, all as u32], "the class migration and the audit migration are applied");

    // What was there is there, and is a function binding.
    assert_eq!(bound_fn(&s, &format!("{HOST}/rate"), END), Some((h.clone(), T1.to_string())));
    assert_eq!(s.resolve_name(&format!("{HOST}/rate"), END).unwrap(), Some(h));
    assert_eq!(s.resolve_class(&format!("{HOST}/rate"), END).unwrap(), None);
    // And classes work on it.
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();
    s.bind_class(CLASS, &c.hash, T2).unwrap();
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), Some(c.hash));
}

// ---- Item 2: class-name bindings ----------------------------------------------------------

fn bind_class_binds_the_name_over_transaction_time(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = fx(CLASS, HOST, "v1");
    let v2 = fx(CLASS, HOST, "v2");
    for c in [&v1, &v2] {
        put_functions(&s, c);
        s.put_class(&c.bytes).unwrap();
    }
    let at = |asof: &str| s.resolve_class(CLASS, asof).unwrap();
    let binding = |asof: &str| s.resolve_class_binding(CLASS, asof).unwrap();

    // Nothing bound yet.
    assert_eq!(at(END), None);
    assert_eq!(binding(END), None);

    // v1 from T1, v2 from T2, v1 again from T3 (a hash the name meant before).
    s.bind_class(CLASS, &v1.hash, T1).unwrap();
    s.bind_class(CLASS, &v2.hash, T2).unwrap();
    s.bind_class(CLASS, &v1.hash, T3).unwrap();

    let pair = |b: Option<ClassBinding>| b.map(|b| (b.class_hash, b.tx));
    assert_eq!(at("2024-03-15T15:59:59.999999Z"), None);
    assert_eq!(at(T1), Some(v1.hash.clone()));
    assert_eq!(pair(binding(T1)), Some((v1.hash.clone(), T1.to_string())));
    assert_eq!(at("2024-03-17T00:00:00.000000Z"), Some(v1.hash.clone()));
    assert_eq!(pair(binding("2024-03-17T00:00:00.000000Z")), Some((v1.hash.clone(), T1.to_string())));
    assert_eq!(at(T2), Some(v2.hash.clone()));
    assert_eq!(at("2024-03-20T08:29:59.999999Z"), Some(v2.hash.clone()));
    assert_eq!(pair(binding(T3)), Some((v1.hash.clone(), T3.to_string())));
    assert_eq!(pair(binding(END)), Some((v1.hash.clone(), T3.to_string())));

    // Append-only in time: not later than the latest is refused, as bind_name does.
    for tx in [T3, T2, T1] {
        match s.bind_class(CLASS, &v2.hash, tx).unwrap_err() {
            Error::NotLater { what, key, given, latest } => {
                assert_eq!((what, key.as_str(), given.as_str(), latest.as_str()), ("name", CLASS, tx, T3));
            }
            e => panic!("not NotLater: {e:?}"),
        }
    }
    assert_eq!(at(END), Some(v1.hash.clone()));

    // Malformed arguments are the core's codes.
    assert_eq!(s.bind_class("Not A Name", &v1.hash, END).unwrap_err().code(), Some(ErrorCode::CanonName));
    assert_eq!(s.bind_class(CLASS, "abc", END).unwrap_err().code(), Some(ErrorCode::CanonHex));
    assert_eq!(s.bind_class(CLASS, &v1.hash, "tomorrow").unwrap_err().code(), Some(ErrorCode::CanonInstant));
    assert_eq!(s.resolve_class("Not A Name", T1).unwrap_err().code(), Some(ErrorCode::CanonName));
    assert_eq!(s.resolve_class(CLASS, "tomorrow").unwrap_err().code(), Some(ErrorCode::CanonInstant));
    assert_eq!(s.resolve_class_binding("Not A Name", T1).unwrap_err().code(), Some(ErrorCode::CanonName));
    assert_eq!(s.resolve_class_binding(CLASS, "tomorrow").unwrap_err().code(), Some(ErrorCode::CanonInstant));
}

fn bind_class_binds_each_method_in_the_same_transaction(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();
    for m in c.methods.keys() {
        assert_eq!(bound_fn(&s, &method_name(CLASS, m), END), None, "{m} before");
    }
    s.bind_class(CLASS, &c.hash, T1).unwrap();
    // N/m means the method's function from the class's tx, and the descriptor is the function's.
    for (m, h) in &c.methods {
        let name = method_name(CLASS, m);
        assert_eq!(bound_fn(&s, &name, T1), Some((h.clone(), T1.to_string())), "{m}");
        assert_eq!(s.resolve_name(&name, END).unwrap().as_deref(), Some(h.as_str()));
        assert!(s.descriptor(h).is_some());
        assert_eq!(bound_fn(&s, &name, "2024-03-15T15:59:59.999999Z"), None, "{m} just before");
    }
    // A method is a function: it is not a class.
    assert_eq!(s.resolve_class(&method_name(CLASS, "rate"), END).unwrap(), None);
}

fn a_subclass_binds_its_inherited_methods_too(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let p = fx_pair();
    let c = fx_forward(&p);
    put_functions(&s, &p);
    put_functions(&s, &c);
    s.put_class(&p.bytes).unwrap();
    s.put_class(&c.bytes).unwrap();
    s.bind_class(CLASS, &p.hash, T1).unwrap();
    s.bind_class(SUB, &c.hash, T2).unwrap();
    // fx_forward/rate and fx_forward/spot are the parent's functions, fx_forward/forward its own.
    for m in ["forward", "rate", "spot"] {
        assert_eq!(bound_fn(&s, &method_name(SUB, m), END), Some((c.methods[m].clone(), T2.to_string())), "{m}");
    }
    assert_eq!(c.methods["rate"], p.methods["rate"]);
    // The parent's names are as they were.
    assert_eq!(bound_fn(&s, &method_name(CLASS, "rate"), END), Some((p.methods["rate"].clone(), T1.to_string())));
    assert_eq!(bound_fn(&s, &method_name(CLASS, "forward"), END), None);
    assert_eq!(s.resolve_class(SUB, END).unwrap(), Some(c.hash.clone()));
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), Some(p.hash));
}

fn binding_a_new_version_of_a_class_rebinds_its_methods_from_that_tx(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = fx(CLASS, HOST, "v1");
    let v2 = fx(CLASS, HOST, "v2");
    for c in [&v1, &v2] {
        put_functions(&s, c);
        s.put_class(&c.bytes).unwrap();
    }
    s.bind_class(CLASS, &v1.hash, T1).unwrap();
    s.bind_class(CLASS, &v2.hash, T2).unwrap();
    let rate = method_name(CLASS, "rate");
    let spot = method_name(CLASS, "spot");
    // `rate` changed: the old function until T2, the new one from T2.
    assert_ne!(v1.methods["rate"], v2.methods["rate"]);
    assert_eq!(bound_fn(&s, &rate, T1), Some((v1.methods["rate"].clone(), T1.to_string())));
    assert_eq!(bound_fn(&s, &rate, "2024-03-18T09:59:59.999999Z"), Some((v1.methods["rate"].clone(), T1.to_string())));
    assert_eq!(bound_fn(&s, &rate, T2), Some((v2.methods["rate"].clone(), T2.to_string())));
    // `spot` kept its function; binding it again begins again at T2.
    assert_eq!(v1.methods["spot"], v2.methods["spot"]);
    assert_eq!(bound_fn(&s, &spot, END), Some((v2.methods["spot"].clone(), T2.to_string())));
}

fn bind_class_needs_the_class_and_every_methods_descriptor_and_binds_nothing_otherwise(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    // The class is not stored.
    assert_eq!(not_found(s.bind_class(CLASS, &c.hash, T1).unwrap_err()), ("class", c.hash.clone()));
    s.put_class(&c.bytes).unwrap();
    // No method descriptor is stored: the first in JCS order of names (`rate`).
    assert_eq!(not_found(s.bind_class(CLASS, &c.hash, T1).unwrap_err()), ("descriptor", c.methods["rate"].clone()));
    // Only `rate`'s is: `spot`'s is missing.
    s.put_descriptor(&c.functions[0]).unwrap();
    assert_eq!(not_found(s.bind_class(CLASS, &c.hash, T1).unwrap_err()), ("descriptor", c.methods["spot"].clone()));
    // Nothing was bound: not the class, not the method that was storable.
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), None);
    assert_eq!(s.resolve_class_binding(CLASS, END).unwrap(), None);
    for m in c.methods.keys() {
        assert_eq!(bound_fn(&s, &method_name(CLASS, m), END), None, "{m}");
    }
    // Once the descriptor is stored the same call at the same tx binds (nothing was used up).
    s.put_descriptor(&c.functions[1]).unwrap();
    s.bind_class(CLASS, &c.hash, T1).unwrap();
    assert_eq!(s.resolve_class(CLASS, T1).unwrap(), Some(c.hash));
}

fn bind_class_is_all_or_nothing_when_a_method_name_cannot_take_the_tx(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();
    // `fx_pair/spot` was bound as a function later than the tx the class is bound at.
    s.bind_name(&method_name(CLASS, "spot"), &c.methods["spot"], T3).unwrap();
    match s.bind_class(CLASS, &c.hash, T2).unwrap_err() {
        Error::NotLater { what, key, given, latest } => {
            assert_eq!(
                (what, key.as_str(), given.as_str(), latest.as_str()),
                ("name", "rates.example.org/fx_pair/spot", T2, T3)
            );
        }
        e => panic!("not NotLater: {e:?}"),
    }
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), None);
    assert_eq!(bound_fn(&s, &method_name(CLASS, "rate"), END), None);
    // Later than every latest: bound.
    let t4 = "2024-03-25T00:00:00.000000Z";
    s.bind_class(CLASS, &c.hash, t4).unwrap();
    assert_eq!(s.resolve_class(CLASS, t4).unwrap(), Some(c.hash.clone()));
    assert_eq!(bound_fn(&s, &method_name(CLASS, "spot"), END), Some((c.methods["spot"].clone(), t4.to_string())));
    // The function binding before it is history.
    assert_eq!(bound_fn(&s, &method_name(CLASS, "spot"), T3), Some((c.methods["spot"].clone(), T3.to_string())));
}

fn a_class_name_that_is_a_function_name_is_refused_and_the_reverse(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();

    // The class name is currently a function name.
    s.bind_name(CLASS, &c.methods["rate"], T1).unwrap();
    assert_eq!(name_taken(s.bind_class(CLASS, &c.hash, T2).unwrap_err()), (CLASS.to_string(), "function"));
    // Nothing was bound, and the function binding is as it was.
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), None);
    assert_eq!(bound_fn(&s, CLASS, END), Some((c.methods["rate"].clone(), T1.to_string())));
    for m in c.methods.keys() {
        assert_eq!(bound_fn(&s, &method_name(CLASS, m), END), None, "{m}");
    }

    // The reverse: a name that is currently a class name cannot be bound as a function.
    let other = "rates.example.org/fx_other";
    s.bind_class(other, &c.hash, T1).unwrap();
    assert_eq!(name_taken(s.bind_name(other, &c.methods["rate"], T2).unwrap_err()), (other.to_string(), "class"));
    assert_eq!(s.resolve_name(other, END).unwrap(), None);
    assert_eq!(s.resolve_class(other, END).unwrap(), Some(c.hash.clone()));

    // A class bound again, to another version, is still a class name.
    let v2 = fx(other, HOST, "v2");
    put_functions(&s, &v2);
    s.put_class(&v2.bytes).unwrap();
    s.bind_class(other, &v2.hash, T2).unwrap();
    assert_eq!(s.resolve_class(other, END).unwrap(), Some(v2.hash));
}

fn a_method_name_that_is_a_class_name_is_refused(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();
    // `rates.example.org/fx_pair/rate` is a class of its own.
    let clash = method_name(CLASS, "rate");
    let other = fx(&clash, HOST, "other");
    put_functions(&s, &other);
    s.put_class(&other.bytes).unwrap();
    s.bind_class(&clash, &other.hash, T1).unwrap();
    assert_eq!(name_taken(s.bind_class(CLASS, &c.hash, T2).unwrap_err()), (clash.clone(), "class"));
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), None);
    assert_eq!(bound_fn(&s, &method_name(CLASS, "spot"), END), None);
    assert_eq!(s.resolve_class(&clash, END).unwrap(), Some(other.hash));
}

fn the_accessors_answer_for_their_own_kind_only(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    put_functions(&s, &c);
    s.put_class(&c.bytes).unwrap();
    let f = "rates.example.org/fx_rate";
    s.bind_name(f, &c.methods["rate"], T1).unwrap();
    s.bind_class(CLASS, &c.hash, T1).unwrap();
    // A class name: only the class accessors answer.
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), Some(c.hash.clone()));
    assert_eq!(s.resolve_name(CLASS, END).unwrap(), None);
    assert_eq!(s.resolve_binding(CLASS, END).unwrap(), None);
    // A function name: the other way.
    assert_eq!(s.resolve_name(f, END).unwrap(), Some(c.methods["rate"].clone()));
    assert_eq!(s.resolve_class(f, END).unwrap(), None);
    assert_eq!(s.resolve_class_binding(f, END).unwrap(), None);
}

fn classes_are_reachable_through_a_trait_object(b: Backend) {
    let db = TestDb::migrated(b);
    // As the engine holds a store: shared, behind a trait object.
    let shared: std::sync::Arc<dyn Store + Send + Sync> = db.bare_store().0;
    let s: &dyn Store = &*shared;
    let c = fx_pair();
    put_functions(s, &c);
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    s.bind_class(CLASS, &c.hash, T1).unwrap();
    assert_eq!(s.resolve_class(CLASS, T1).unwrap(), Some(c.hash.clone()));
    assert_eq!(s.class(&c.hash).unwrap(), Some(c.bytes));
}

// ---- Item 4: the owner is the definer's check ---------------------------------------------

fn the_store_records_the_owner_and_does_not_check_it(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    // A class stewarded by one namespace, bound under a name of another host, with no
    // namespace recorded for either: 16.2 step 1 (E_CLASS_OWNER) is define's, so the store
    // takes it, and put_class does not need a key document.
    let c = fx(CLASS, "stewards.example.net", "v1");
    put_functions(&s, &c);
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    let foreign = "someone.example.com/fx_pair";
    s.bind_class(foreign, &c.hash, T1).unwrap();
    assert_eq!(s.resolve_class(foreign, T1).unwrap(), Some(c.hash.clone()));
    assert_eq!(bound_fn(&s, &method_name(foreign, "rate"), T1), Some((c.methods["rate"].clone(), T1.to_string())));
    // And its methods are bound under the class name's host, whatever the steward says.
    assert_eq!(s.namespace("stewards.example.net").unwrap(), None);
    assert_eq!(s.namespace("someone.example.com").unwrap(), None);
}

// ---- Item 3: suspension (step 1.5) --------------------------------------------------------

fn put_class_under_a_suspended_namespace_is_refused_and_class_reads_go_on(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    let stored = fx(CLASS, HOST, "v2");
    s.put_class(&stored.bytes).unwrap();
    suspend(&s, HOST, T2);

    // The steward's namespace is suspended: a new class is refused, and not stored.
    assert_eq!(suspended(s.put_class(&c.bytes).unwrap_err()), (HOST.to_string(), T2.to_string()));
    assert_eq!(s.class(&c.hash).unwrap(), None);
    // Reads are never refused: the class stored before.
    assert_eq!(s.class(&stored.hash).unwrap(), Some(stored.bytes.clone()));
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), None);
    // Reinstated: the write goes through.
    s.record_namespace_success(HOST, T3).unwrap();
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
}

/// A class already stored is refused again while the steward is suspended: `put_class` is a
/// write under the namespace whatever the bytes (unlike `put_receipt` of a stored receipt,
/// which is a read of what verified). Isolated so that the answer is easy to change.
fn putting_a_stored_class_again_under_a_suspended_namespace_is_refused(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let c = fx_pair();
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    suspend(&s, HOST, T2);
    assert_eq!(suspended(s.put_class(&c.bytes).unwrap_err()), (HOST.to_string(), T2.to_string()));
    assert_eq!(s.class(&c.hash).unwrap(), Some(c.bytes));
}

fn bind_class_under_a_suspended_namespace_is_refused_and_reads_go_on(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = fx(CLASS, HOST, "v1");
    let v2 = fx(CLASS, HOST, "v2");
    for c in [&v1, &v2] {
        put_functions(&s, c);
        s.put_class(&c.bytes).unwrap();
    }
    s.bind_class(CLASS, &v1.hash, T1).unwrap();
    suspend(&s, HOST, T2);

    // A new binding of the name, with its methods: refused, nothing bound.
    assert_eq!(suspended(s.bind_class(CLASS, &v2.hash, T3).unwrap_err()), (HOST.to_string(), T2.to_string()));
    assert_eq!(s.resolve_class(CLASS, END).unwrap(), Some(v1.hash.clone()));
    assert_eq!(bound_fn(&s, &method_name(CLASS, "rate"), END), Some((v1.methods["rate"].clone(), T1.to_string())));
    // A function name under the host is refused as before, a class name too.
    let other = "rates.example.org/fx_other";
    assert_eq!(suspended(s.bind_class(other, &v1.hash, T3).unwrap_err()), (HOST.to_string(), T2.to_string()));
    assert_eq!(s.resolve_class(other, END).unwrap(), None);
    // Reads: the class, the binding as of any time, the method bindings, the descriptors.
    assert_eq!(s.class(&v1.hash).unwrap(), Some(v1.bytes.clone()));
    assert_eq!(s.resolve_class(CLASS, T1).unwrap(), Some(v1.hash.clone()));
    assert_eq!(s.resolve_class_binding(CLASS, END).unwrap().unwrap().tx, T1);
    assert!(s.resolve_name(&method_name(CLASS, "spot"), END).unwrap().is_some());

    // Reinstated: the same call goes through.
    s.record_namespace_success(HOST, T3).unwrap();
    let t4 = "2024-03-25T00:00:00.000000Z";
    s.bind_class(CLASS, &v2.hash, t4).unwrap();
    assert_eq!(s.resolve_class(CLASS, t4).unwrap(), Some(v2.hash));
}

fn suspension_is_per_host_for_classes(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    // Another host is suspended: this class, its steward and its name are unaffected.
    suspend(&s, "calc.example.net", T2);
    let c = fx_pair();
    put_functions(&s, &c);
    assert_eq!(s.put_class(&c.bytes).unwrap(), c.hash);
    s.bind_class(CLASS, &c.hash, T3).unwrap();
    // A class whose steward is the suspended host is refused, whoever binds it where.
    let foreign = fx("rates.example.org/fx_theirs", "calc.example.net", "v1");
    assert_eq!(suspended(s.put_class(&foreign.bytes).unwrap_err()), ("calc.example.net".to_string(), T2.to_string()));
    // A class name under the suspended host is refused (its class is stored: it is
    // another host's steward that put it, before).
    let db2 = TestDb::migrated(b);
    let s2 = db2.bare_store();
    let theirs = fx("calc.example.net/fx_pair", "calc.example.net", "v1");
    put_functions(&s2, &theirs);
    s2.put_class(&theirs.bytes).unwrap();
    suspend(&s2, "calc.example.net", T2);
    assert_eq!(
        suspended(s2.bind_class("calc.example.net/fx_pair", &theirs.hash, T3).unwrap_err()),
        ("calc.example.net".to_string(), T2.to_string())
    );
}

backends!(
    put_class_stores_the_flattened_descriptor_by_its_class_hash,
    put_class_is_idempotent_on_the_same_bytes,
    put_class_stores_a_subclass_by_its_flattened_hash,
    put_class_rejects_bytes_that_are_not_canonical,
    put_class_rejects_what_the_core_rejects,
    class_hashes_and_function_hashes_are_separate_rows,
    the_class_migration_is_additive_and_keeps_function_bindings,
    bind_class_binds_the_name_over_transaction_time,
    bind_class_binds_each_method_in_the_same_transaction,
    a_subclass_binds_its_inherited_methods_too,
    binding_a_new_version_of_a_class_rebinds_its_methods_from_that_tx,
    bind_class_needs_the_class_and_every_methods_descriptor_and_binds_nothing_otherwise,
    bind_class_is_all_or_nothing_when_a_method_name_cannot_take_the_tx,
    a_class_name_that_is_a_function_name_is_refused_and_the_reverse,
    a_method_name_that_is_a_class_name_is_refused,
    the_accessors_answer_for_their_own_kind_only,
    classes_are_reachable_through_a_trait_object,
    the_store_records_the_owner_and_does_not_check_it,
    put_class_under_a_suspended_namespace_is_refused_and_class_reads_go_on,
    putting_a_stored_class_again_under_a_suspended_namespace_is_refused,
    bind_class_under_a_suspended_namespace_is_refused_and_reads_go_on,
    suspension_is_per_host_for_classes,
);
