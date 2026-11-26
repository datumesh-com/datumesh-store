//! Catalogue facets and `extends` (CONTRACT-classes-round C2 and K9; migration 0012 on PostgreSQL,
//! 0008 on SQLite). Both backends.
//!
//! API specified by these tests. It is ADDITIVE: `CatalogEntry` is not touched (it is a public
//! struct without `#[non_exhaustive]`, so a new field would be a breaking change), and the
//! new `Store` methods have default bodies (`Err(Error::Unsupported)`), as 0.5.2's did.
//!
//! ```text
//! pub type Facets = BTreeMap<String, Vec<String>>;      // facet key -> names, ascending, unique
//!
//! #[non_exhaustive] #[derive(Clone, Debug, PartialEq, Eq)]
//! pub struct CatalogEntryExt { pub entry: CatalogEntry, pub facets: Facets, pub extends: Option<String> }
//! impl CatalogEntryExt { pub fn new(entry: CatalogEntry) -> Self }   // facets empty, extends None
//!
//! #[non_exhaustive] #[derive(Clone, Debug, PartialEq, Eq)]
//! pub struct FacetedEntry { pub kind: CatalogKind, pub name: String, pub hash: Option<String>,
//!                           pub facets: Facets, pub first_bound: Option<String> }
//!
//! // on Store:
//! fn upsert_catalog_ext(&self, entries: &[CatalogEntryExt]) -> Result<Vec<i64>>;
//! fn catalog_entry_ext(&self, kind: CatalogKind, name: &str) -> Result<Option<CatalogEntryExt>>;
//! fn catalog_entries_with_facets(&self) -> Result<Vec<FacetedEntry>>;   // by kind (text), then name
//! fn extensions_of(&self, class: &str) -> Result<Vec<CatalogEntryExt>>; // functions, by name
//! ```
//!
//! Rules:
//! - `upsert_catalog_ext` is `upsert_catalog` plus the two columns; it REPLACES facets and extends
//!   of an existing row. The 0.5.2 `upsert_catalog` leaves them of an existing row unchanged
//!   (a new row gets `{}` and NULL), so an old caller never wipes them.
//! - The STORE refuses (Error::Invalid, nothing of the call written): a facet key other than
//!   domain, thing, measure, region, frequency; a facet value that is not a name (2.2) of at most
//!   32 bytes; a list of 0 or more than 8 values, or not ascending and unique; `extends` that is
//!   not a full name, or on an entry whose kind is not function. http checks the same first and
//!   answers with its own code; the store does not trust it. The store does NOT check that
//!   `extends` names a stored class, nor the key rule of 16.3 (that is the publish check, http).
//! - `first_bound` is the instant of the earliest binding of the name (`bind_name` for functions
//!   and methods, `bind_class` for classes); None when the name was never bound.
//! - `catalog_entries_with_facets` lists only entries with at least one facet key, by kind (as
//!   text) then name in byte order.

#[macro_use]
mod common;

use std::collections::BTreeMap;

use common::*;
use datumesh_core::{ClassEnv, FunctionDescriptor};
use datumesh_store::{migrate, CatalogEntry, CatalogEntryExt, CatalogKind, CatalogSource, Error, Facets, Store};

const F1: &str = "rates.example.org/fx_rate";
const F2: &str = "rates.example.org/fx_cross";
const F3: &str = "other.example.net/fx_other";
const CLASS: &str = "rates.example.org/fx_pair";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn entry(kind: CatalogKind, name: &str) -> CatalogEntry {
    CatalogEntry {
        kind,
        name: name.to_string(),
        hash: None,
        label: "l".to_string(),
        description: "d".to_string(),
        args: BTreeMap::new(),
        examples: "[]".to_string(),
        source: CatalogSource::Publisher,
        signed_by: Some("rates.example.org".to_string()),
    }
}

fn facets(pairs: &[(&str, &[&str])]) -> Facets {
    pairs.iter().map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect())).collect()
}

fn ext(kind: CatalogKind, name: &str, f: Facets, extends: Option<&str>) -> CatalogEntryExt {
    let mut e = CatalogEntryExt::new(entry(kind, name));
    e.facets = f;
    e.extends = extends.map(str::to_string);
    e
}

fn all_facets() -> Facets {
    facets(&[
        ("domain", &["finance"]),
        ("frequency", &["daily", "on_change"]),
        ("measure", &["exchange_rate"]),
        ("region", &["ea", "none"]),
        ("thing", &["currency"]),
    ])
}

/// A minimal class under `rates.example.org` (as classes.rs builds `fx_pair`: two upper_ascii keys,
/// one method `rate`), with the method function's descriptor. Returns (class bytes, class hash,
/// method descriptor).
fn minimal_class() -> (Vec<u8>, String, Vec<u8>) {
    let key = r#"{"key_normalisation":"upper_ascii","type":"string","valid":false}"#;
    let rate = jcs(format!(
        r#"{{"code":{{"body":"rate","lang":"stored"}},"inputs":[],"signature":{{"args":{{"base":{key},"date":{{"type":"date","valid":true}},"quote":{key}}},"order":["base","quote","date"],"returns":{{"type":"number","unit":"1"}}}}}}"#
    )
    .as_bytes());
    let text = format!(
        r#"{{"steward":{{"process":"https://rates.example.org/classes/change","namespace":"rates.example.org"}},"order":["base","quote"],"methods":{{"rate":"{}"}},"keys":{{"quote":{{"type":"string","key_normalisation":"upper_ascii"}},"base":{{"type":"string","key_normalisation":"upper_ascii"}}}}}}"#,
        fn_hash_of(&rate)
    );
    let mut env = ClassEnv::new("rates.example.org");
    env.insert_function(FunctionDescriptor::from_json(&rate).unwrap());
    let flat = datumesh_core::define_class(CLASS, text.as_bytes(), &env).unwrap();
    (flat.canonical(), flat.hash(), rate)
}

fn bind_function(s: &dyn Store, name: &str, tx: &str) -> String {
    let h = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(name, &h, tx).unwrap();
    h
}

// ---- C2 ------------------------------------------------------------------------------------

fn facets_round_trip(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let mut e = ext(CatalogKind::Function, F1, all_facets(), None);
    e.entry.hash = Some(HASH_A.to_string());
    s.upsert_catalog_ext(std::slice::from_ref(&e)).unwrap();
    assert_eq!(s.catalog_entry_ext(CatalogKind::Function, F1).unwrap(), Some(e));
    assert_eq!(s.catalog_entry_ext(CatalogKind::Class, F1).unwrap(), None);
    // The stored text is JSON of the map, keys ascending (the column is text).
    assert_eq!(
        db.texts("SELECT facets FROM catalog_entries")[0][0],
        r#"{"domain":["finance"],"frequency":["daily","on_change"],"measure":["exchange_rate"],"region":["ea","none"],"thing":["currency"]}"#
    );
    // A class entry carries facets too; the plain entry read still works.
    let c = ext(CatalogKind::Class, CLASS, facets(&[("domain", &["finance"])]), None);
    s.upsert_catalog_ext(std::slice::from_ref(&c)).unwrap();
    assert_eq!(s.catalog_entry_ext(CatalogKind::Class, CLASS).unwrap(), Some(c));
    assert_eq!(s.catalog_entry(F1).unwrap().unwrap().name, F1);
}

fn an_entry_without_facets_reads_empty(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    // Written by the 0.5.2 call, and by the new one with nothing.
    s.upsert_catalog(&[entry(CatalogKind::Function, F1)]).unwrap();
    s.upsert_catalog_ext(&[CatalogEntryExt::new(entry(CatalogKind::Function, F2))]).unwrap();
    for n in [F1, F2] {
        let e = s.catalog_entry_ext(CatalogKind::Function, n).unwrap().unwrap();
        assert!(e.facets.is_empty());
        assert_eq!(e.extends, None);
    }
    assert_eq!(db.texts("SELECT facets FROM catalog_entries ORDER BY name"), [["{}"], ["{}"]]);
    assert!(s.catalog_entries_with_facets().unwrap().is_empty());
}

fn an_update_replaces_facets(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.upsert_catalog_ext(&[ext(CatalogKind::Function, F1, all_facets(), None)]).unwrap();
    let id2 = s.upsert_catalog_ext(&[ext(CatalogKind::Function, F1, facets(&[("region", &["us"])]), None)]).unwrap();
    assert_eq!(db.count("catalog_entries"), 1);
    assert_eq!(id2.len(), 1);
    let e = s.catalog_entry_ext(CatalogKind::Function, F1).unwrap().unwrap();
    assert_eq!(e.facets, facets(&[("region", &["us"])]), "replaced, not merged");
    // Replacing with none clears them.
    s.upsert_catalog_ext(&[CatalogEntryExt::new(entry(CatalogKind::Function, F1))]).unwrap();
    assert!(s.catalog_entry_ext(CatalogKind::Function, F1).unwrap().unwrap().facets.is_empty());
    assert!(s.catalog_entries_with_facets().unwrap().is_empty());
}

fn the_old_upsert_leaves_facets_and_extends_alone(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let e = ext(CatalogKind::Function, F1, all_facets(), Some(CLASS));
    s.upsert_catalog_ext(std::slice::from_ref(&e)).unwrap();
    let mut plain = entry(CatalogKind::Function, F1);
    plain.label = "changed".to_string();
    s.upsert_catalog(&[plain]).unwrap();
    let got = s.catalog_entry_ext(CatalogKind::Function, F1).unwrap().unwrap();
    assert_eq!(got.entry.label, "changed");
    assert_eq!(got.facets, e.facets);
    assert_eq!(got.extends.as_deref(), Some(CLASS));
}

fn entries_with_facets_are_listed_with_their_first_binding_time(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let h = bind_function(&*s, F1, T1);
    // Bound again later (same hash): the first time is still T1.
    s.bind_name(F1, &h, T2).unwrap();
    bind_function(&*s, F2, T2);
    let mut e1 = ext(CatalogKind::Function, F1, all_facets(), None);
    e1.entry.hash = Some(h.clone());
    let e2 = ext(CatalogKind::Function, F2, facets(&[("domain", &["finance"])]), None);
    let e3 = ext(CatalogKind::Function, F3, facets(&[("thing", &["currency"])]), None); // never bound
                                                                                        // The class is bound at T2 (bind_class), which is its first binding time.
    let (class_bytes, class_hash, rate) = minimal_class();
    s.put_descriptor(&rate).unwrap();
    s.put_class(&class_bytes).unwrap();
    s.bind_class(CLASS, &class_hash, T2).unwrap();
    let c = ext(CatalogKind::Class, CLASS, facets(&[("domain", &["finance"])]), None);
    let none = CatalogEntryExt::new(entry(CatalogKind::Function, "rates.example.org/plain"));
    s.upsert_catalog_ext(&[e3, none, c, e2, e1]).unwrap();

    let got = s.catalog_entries_with_facets().unwrap();
    let view: Vec<_> =
        got.iter().map(|f| (f.kind, f.name.as_str(), f.hash.clone(), f.first_bound.clone(), f.facets.len())).collect();
    // By kind (as text: class, function, method), then name in byte order; entries without
    // facets are left out.
    assert_eq!(
        view,
        vec![
            (CatalogKind::Class, CLASS, None, Some(T2.to_string()), 1),
            (CatalogKind::Function, F3, None, None, 1),
            (CatalogKind::Function, F2, None, Some(T2.to_string()), 1),
            (CatalogKind::Function, F1, Some(h), Some(T1.to_string()), 5),
        ]
    );
    assert_eq!(got[3].facets, all_facets());
}

fn the_store_refuses_bad_facets_and_writes_nothing(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let good = ext(CatalogKind::Function, F2, facets(&[("domain", &["finance"])]), None);
    let long = "x".repeat(33);
    let nine: Vec<String> = (1..=9).map(|i| format!("v{i}")).collect();
    let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
    let bad: Vec<(&str, Facets)> = vec![
        ("unknown key", facets(&[("colour", &["red"])])),
        ("empty list", facets(&[("domain", &[])])),
        ("nine values", facets(&[("domain", &nine)])),
        ("descending", facets(&[("domain", &["b", "a"])])),
        ("duplicate", facets(&[("domain", &["a", "a"])])),
        ("33 bytes", facets(&[("domain", &[long.as_str()])])),
        ("not a name", facets(&[("domain", &["Not A Name"])])),
        ("empty value", facets(&[("region", &[""])])),
    ];
    for (what, f) in bad {
        let r = s.upsert_catalog_ext(&[good.clone(), ext(CatalogKind::Function, F1, f, None)]);
        assert!(matches!(r, Err(Error::Invalid(_))), "{what}: {r:?}");
        assert_eq!(db.count("catalog_entries"), 0, "{what}: nothing written");
    }
    // The bounds at their edges are accepted: 32 bytes, 8 values, all five keys.
    let edge = "y".repeat(32);
    let eight = ["a", "b", "c", "d", "e", "f", "g", "h"];
    s.upsert_catalog_ext(&[ext(
        CatalogKind::Function,
        F1,
        facets(&[("domain", &[edge.as_str()]), ("thing", &eight)]),
        None,
    )])
    .unwrap();
}

fn the_database_refuses_a_facets_column_that_is_not_json_text(b: Backend) {
    let db = TestDb::migrated(b);
    let r = db.exec(
        "INSERT INTO catalog_entries (kind, name, label, description, source, facets) \
         VALUES ('function', 'x.example.org/f', 'l', 'd', 'operator', 'not json')",
    );
    assert!(r.is_err(), "facets is a JSON object");
}

// ---- K9 ------------------------------------------------------------------------------------

fn extends_round_trips(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let e = ext(CatalogKind::Function, F1, Facets::new(), Some(CLASS));
    s.upsert_catalog_ext(std::slice::from_ref(&e)).unwrap();
    assert_eq!(s.catalog_entry_ext(CatalogKind::Function, F1).unwrap(), Some(e));
    // Cleared by an update without it.
    s.upsert_catalog_ext(&[CatalogEntryExt::new(entry(CatalogKind::Function, F1))]).unwrap();
    assert_eq!(s.catalog_entry_ext(CatalogKind::Function, F1).unwrap().unwrap().extends, None);
}

fn extensions_of_lists_the_functions_extending_a_class(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let other = "rates.example.org/fx_triple";
    s.upsert_catalog_ext(&[
        ext(CatalogKind::Function, F3, Facets::new(), Some(CLASS)), // another namespace: still listed
        ext(CatalogKind::Function, F1, Facets::new(), Some(CLASS)),
        ext(CatalogKind::Function, F2, Facets::new(), Some(other)),
        ext(CatalogKind::Function, "rates.example.org/plain", Facets::new(), None),
    ])
    .unwrap();
    let names = |c: &str| -> Vec<String> { s.extensions_of(c).unwrap().into_iter().map(|e| e.entry.name).collect() };
    assert_eq!(names(CLASS), [F3, F1], "ascending by name (bytes)");
    assert_eq!(names(other), [F2]);
    assert!(names("rates.example.org/nobody").is_empty());
    let e = &s.extensions_of(CLASS).unwrap()[0];
    assert_eq!(e.extends.as_deref(), Some(CLASS));
    assert_eq!(e.entry.signed_by.as_deref(), Some("rates.example.org"));
    // A class name that is not a full name is refused, not answered empty.
    assert!(matches!(s.extensions_of("Not A Name"), Err(Error::Invalid(_))));
}

fn the_store_refuses_a_bad_extends(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    for bad in ["", "no-slash", "Rates.Example.Org/x y", &"a/".repeat(300)] {
        let r = s.upsert_catalog_ext(&[ext(CatalogKind::Function, F1, Facets::new(), Some(bad))]);
        assert!(matches!(r, Err(Error::Invalid(_))), "{bad:?}: {r:?}");
    }
    // Only a function extends a class.
    for k in [CatalogKind::Class, CatalogKind::Method] {
        let r = s.upsert_catalog_ext(&[ext(k, "rates.example.org/fx_pair/m", Facets::new(), Some(CLASS))]);
        assert!(matches!(r, Err(Error::Invalid(_))), "{k:?}: {r:?}");
    }
    assert_eq!(db.count("catalog_entries"), 0);
}

// ---- Migrations ------------------------------------------------------------------------------

fn columns(db: &TestDb) -> Vec<String> {
    let pg = "SELECT column_name::text FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'catalog_entries' ORDER BY column_name";
    let lite = "SELECT name FROM pragma_table_info('catalog_entries') ORDER BY name";
    db.texts(db.pick(pg, lite)).into_iter().map(|r| r[0].clone()).collect()
}

fn the_migrations_are_registered(b: Backend) {
    let ms = match b {
        Backend::Pg => migrate::MIGRATIONS,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS,
    };
    let (v, n) = match b {
        Backend::Pg => (13, "0013_catalogue_facets.sql"),
        Backend::Sqlite => (9, "0009_catalogue_facets.sql"),
    };
    let m = &ms[v as usize - 1];
    assert_eq!((ms.len() as u32, m.version, m.name), (v + 3, v, n)); // withdrawals (14/10), sort keys (15/11) and member sets (16/12) follow
    let db = TestDb::migrated(b);
    let c = columns(&db);
    assert!(c.contains(&"facets".to_string()) && c.contains(&"extends".to_string()), "{c:?}");
}

fn a_database_at_0_5_2_gets_the_defaults(b: Backend) {
    let db = TestDb::empty(b);
    let (old, all): (Vec<&migrate::Migration>, usize) = match b {
        Backend::Pg => (migrate::MIGRATIONS[..11].iter().collect(), migrate::MIGRATIONS.len()),
        Backend::Sqlite => (migrate::sqlite::MIGRATIONS[..7].iter().collect(), migrate::sqlite::MIGRATIONS.len()),
    };
    assert_eq!(
        all,
        old.len() + 4,
        "four migrations more than 0.5.2 on each backend (0.5.3's counters, facets, withdrawals, sort keys)"
    );
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
    assert!(!columns(&db).contains(&"facets".to_string()));
    for (k, n) in [("function", F1), ("class", CLASS)] {
        db.exec(&format!(
            "INSERT INTO catalog_entries (kind, name, label, description, args, examples, source) \
             VALUES ('{k}', '{n}', 'l', 'd', '{{}}', '[]', 'publisher')"
        ))
        .unwrap();
    }
    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(datumesh_store::SqliteOptions::new())).unwrap(),
    };
    assert_eq!(
        applied,
        [all as u32 - 3, all as u32 - 2, all as u32 - 1, all as u32],
        "only the counters, the facets, the withdrawals and the sort-key migration are applied"
    );
    assert_eq!(db.count("catalog_entries"), 2, "old rows kept");
    assert_eq!(db.int("SELECT count(*) FROM catalog_entries WHERE facets = '{}' AND extends IS NULL"), 2);
    let s = db.bare_store();
    let e = s.catalog_entry_ext(CatalogKind::Function, F1).unwrap().unwrap();
    assert!(e.facets.is_empty() && e.extends.is_none());
    assert!(s.catalog_entries_with_facets().unwrap().is_empty());
    // And the new calls work on the migrated database.
    s.upsert_catalog_ext(&[ext(CatalogKind::Function, F1, all_facets(), Some(CLASS))]).unwrap();
    assert_eq!(s.catalog_entries_with_facets().unwrap().len(), 1);
    // Re-running is a no-op.
    let again = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(datumesh_store::SqliteOptions::new())).unwrap(),
    };
    assert!(again.is_empty());
}

backends!(
    facets_round_trip,
    an_entry_without_facets_reads_empty,
    an_update_replaces_facets,
    the_old_upsert_leaves_facets_and_extends_alone,
    entries_with_facets_are_listed_with_their_first_binding_time,
    the_store_refuses_bad_facets_and_writes_nothing,
    the_database_refuses_a_facets_column_that_is_not_json_text,
    extends_round_trips,
    extensions_of_lists_the_functions_extending_a_class,
    the_store_refuses_a_bad_extends,
    the_migrations_are_registered,
    a_database_at_0_5_2_gets_the_defaults,
);
