//! Store 0.7.0, the listing release (CONTRACT-http-listing.md L3, approved 25 Nov with eight changes): the tables
//! `listings` and `listed_hosts`, the catalogue source `listed`, and the store calls that write and read them. Both
//! backends. These tests fail on the stubs of the test commit (every listing call is `Error::Unsupported`, and there is no
//! migration 18 / SQLite 14); the implementer makes them pass without editing them.
//!
//! API (defined by the test writer; the shapes are in `src/listings.rs` and the `Store` trait's docs):
//!
//! ```text
//! pub struct SubjectKeys { pub domain, pub thing, pub measure, pub region: String }   // Ord: domain, thing, measure, region
//! pub struct Listing { pub host, pub name, pub fn_hash: String, pub subjects: Vec<SubjectKeys>, pub descriptor: Vec<u8>,
//!                      pub key_id, pub label, pub description, pub fetched_at, pub listed_at: String }
//! pub enum ListingWrite { Unchanged, Inserted, Changed }
//! pub struct ListedHost { pub host: String, pub last_seen: Option<String> /* a day */, pub unreachable_since: Option<String> }
//! pub enum CatalogSource { Publisher, Operator, Listed }                               // `listed`
//! fn put_listing(&self, l: &Listing) -> Result<ListingWrite>;
//! fn listing(&self, name: &str) -> Result<Option<Listing>>;
//! fn listings_of(&self, host: &str) -> Result<Vec<Listing>>;                         // by name
//! fn listing_count(&self) -> Result<u64>;
//! fn delete_listing(&self, name: &str) -> Result<bool>;
//! fn put_listed_host(&self, h: &ListedHost) -> Result<bool>;                          // false: unchanged, nothing written
//! fn listed_host(&self, host: &str) -> Result<Option<ListedHost>>;
//! fn listed_hosts(&self) -> Result<Vec<ListedHost>>;                                  // by host
//! fn delete_listed_host(&self, host: &str) -> Result<bool>;                           // and the host's listings
//! ```
//!
//! Migrations: PostgreSQL 18 `0018_listings.sql`, SQLite 14 `0014_listings.sql`. Additive: no stored row changes.

#[macro_use]
mod common;

use common::*;
use datumesh_core::Type;
use datumesh_store::{
    migrate, CatalogKind, CatalogSource, Error, ListedHost, Listing, ListingWrite, SubjectKeys, LISTING_SUBJECTS_MAX,
};

const SRC: &str = "source.example";
const T1: &str = "2026-11-16T00:00:00.000000Z";
const T2: &str = "2026-11-16T06:00:00.000000Z";
const T3: &str = "2026-11-17T00:00:00.000000Z";

fn bank_rate() -> String {
    format!("{SRC}/bank_rate")
}

fn desc(body: &str) -> Vec<u8> {
    stored_descriptor(&[("date", Type::Date)], body)
}

fn gb() -> SubjectKeys {
    SubjectKeys::new("finance", "rate", "interest_rate", "gb")
}

fn listing(name: &str, body: &str, at: &str) -> Listing {
    let d = desc(body);
    Listing {
        host: SRC.to_string(),
        name: name.to_string(),
        fn_hash: fn_hash_of(&d),
        subjects: vec![gb()],
        descriptor: d,
        key_id: format!("{SRC}#k1"),
        label: "Bank Rate".to_string(),
        description: "The official bank rate of the source, as its own mesh publishes it.".to_string(),
        fetched_at: at.to_string(),
        listed_at: at.to_string(),
    }
}

fn invalid(r: datumesh_store::Result<ListingWrite>, what: &str) {
    match r {
        Err(Error::Invalid(_)) => {}
        other => panic!("{what}: want Error::Invalid, got {other:?}"),
    }
}

/// The rows of the tables a listing must never touch, as text.
fn untouched(db: &TestDb) -> Vec<i64> {
    ["descriptors", "receipts", "versions", "cells", "name_bindings", "namespaces", "class_bindings"]
        .iter()
        .map(|t| db.count(t))
        .collect()
}

fn updated_at(db: &TestDb, name: &str) -> String {
    db.texts(&format!(
        "SELECT CAST(updated_at AS TEXT) FROM catalog_entries WHERE kind = 'function' AND name = '{name}'"
    ))
    .into_iter()
    .next()
    .unwrap_or_else(|| panic!("no catalogue entry for {name}"))
    .remove(0)
}

// ---- the migration -----------------------------------------------------------------------------------------------

/// Migration 18 (SQLite 14), the listings, at its place; later releases add more after it (0.7.2 adds the per-name
/// unreached clock, `store_072_listing_unreached.rs`, which states the latest).
fn migration_18_and_14_are_the_last_and_named(b: Backend) {
    let ms = match b {
        Backend::Pg => migrate::MIGRATIONS,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS,
    };
    let want = if b == Backend::Pg { (18, "0018_listings.sql") } else { (14, "0014_listings.sql") };
    let m = &ms[want.0 as usize - 1];
    assert_eq!((m.version, m.name), want);
    assert!(ms.len() as u32 >= want.0);
}

fn the_tables_have_the_contracts_columns(b: Backend) {
    let db = TestDb::migrated(b);
    let cols = |t: &str| -> Vec<String> {
        let sql = db
            .pick(
                &format!(
                    "SELECT column_name::text FROM information_schema.columns WHERE table_name = '{t}' ORDER BY 1"
                ),
                &format!("SELECT name FROM pragma_table_info('{t}') ORDER BY 1"),
            )
            .to_string();
        db.texts(&sql).into_iter().map(|mut r| r.remove(0)).collect()
    };
    let listings = cols("listings");
    for c in [
        "host",
        "name",
        "fn_hash",
        "subjects",
        "descriptor",
        "key_id",
        "fetched_at",
        "listed_at",
        "label",
        "description",
    ] {
        assert!(listings.contains(&c.to_string()), "listings has {c}: {listings:?}");
    }
    let hosts = cols("listed_hosts");
    for c in ["host", "last_seen", "unreachable_since"] {
        assert!(hosts.contains(&c.to_string()), "listed_hosts has {c}: {hosts:?}");
    }
    assert_eq!(db.count("listings"), 0);
    assert_eq!(db.count("listed_hosts"), 0);
}

/// The migration is additive: a database at 0.6.5's schema keeps every row it had, byte for byte, and exactly one migration
/// is applied.
fn the_migration_changes_no_stored_row(b: Backend) {
    let db = TestDb::empty(b);
    let old: Vec<&migrate::Migration> = match b {
        Backend::Pg => migrate::MIGRATIONS[..17].iter().collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS[..13].iter().collect(),
    };
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
    {
        // Rows written by the store on the 0.6.5 schema: a key document, a function, a binding, a stored cell, a catalogue
        // entry with facets.
        let s = db.bare_store();
        s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
        let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
        s.bind_name(&format!("{SLICE_HOST}/f"), &h, VALIDATED_AT).unwrap();
        put_member(&s, &slice_cell("f", &h, &[("k", sc(Type::Number, "1"))]), "1", T1, T1);
        let mut e = datumesh_store::CatalogEntryExt::new(datumesh_store::CatalogEntry {
            kind: CatalogKind::Function,
            name: format!("{SLICE_HOST}/f"),
            hash: Some(h.clone()),
            label: "F".into(),
            description: "A function.".into(),
            args: Default::default(),
            examples: "[]".into(),
            source: CatalogSource::Publisher,
            signed_by: None,
        });
        e.facets.insert("domain".into(), vec!["finance".into()]);
        s.upsert_catalog_ext(&[e]).unwrap();
    }
    let snapshot = |db: &TestDb| -> Vec<Vec<Vec<String>>> {
        [
            "SELECT fn_hash FROM descriptors ORDER BY 1",
            "SELECT hash FROM receipts ORDER BY 1",
            "SELECT cell_id || ' ' || tx || ' ' || receipt_hash FROM versions ORDER BY 1",
            "SELECT fn_name || ' ' || tx || ' ' || COALESCE(fn_hash, '') FROM name_bindings ORDER BY 1",
            "SELECT kind || ' ' || name || ' ' || source || ' ' || CAST(updated_at AS TEXT) FROM catalog_entries ORDER BY 1",
            "SELECT host FROM namespaces ORDER BY 1",
        ]
        .iter()
        .map(|q| db.texts(q))
        .collect()
    };
    let before = snapshot(&db);
    assert!(before.iter().all(|t| !t.is_empty()), "the fixture wrote a row to every table: {before:?}");
    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(datumesh_store::SqliteOptions::new())).unwrap(),
    };
    assert_eq!(applied.first(), Some(&(old.len() as u32 + 1)), "the listing migration first (later ones follow)");
    assert_eq!(
        applied,
        (old.len() as u32 + 1..=match b {
            Backend::Pg => migrate::MIGRATIONS.len() as u32,
            Backend::Sqlite => migrate::sqlite::MIGRATIONS.len() as u32,
        })
            .collect::<Vec<u32>>()
    );
    assert_eq!(snapshot(&db), before, "no stored row changes");
}

/// `catalog_entries.source` takes `listed`, written by the store's listing call (and only `publisher`, `operator`,
/// `listed`).
fn the_catalogue_source_takes_listed(b: Backend) {
    assert_eq!(CatalogSource::Listed.as_str(), "listed");
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listing(&listing(&bank_rate(), "a", T1)).unwrap();
    assert_eq!(
        db.texts(&format!("SELECT source FROM catalog_entries WHERE name = '{}'", bank_rate())),
        vec![vec!["listed".to_string()]]
    );
    let err = db
        .exec("INSERT INTO catalog_entries (kind, name, label, description, source) VALUES ('function', 'x.example/f', '', 'd', 'other')")
        .expect_err("a source other than publisher, operator or listed is refused");
    assert!(!err.is_empty());
}

// ---- writing and reading a listing ------------------------------------------------------------------------------

fn a_listing_is_written_with_its_descriptor_and_catalogue_entry(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let l = listing(&bank_rate(), "a", T1);
    assert_eq!(s.put_listing(&l).unwrap(), ListingWrite::Inserted);
    assert_eq!(s.listing(&bank_rate()).unwrap(), Some(l.clone()));
    assert_eq!(s.listing(&format!("{SRC}/other")).unwrap(), None);
    // the descriptor answers by its hash (`/descriptor/<hash>` reads `descriptors`)
    assert_eq!(s.descriptor(&l.fn_hash).as_deref(), Some(l.descriptor.as_slice()));
    // the catalogue entry: kind function, the listing's text, source listed, signed by the host, the subject's facets
    let e = s.catalog_entry_ext(CatalogKind::Function, &bank_rate()).unwrap().expect("a catalogue entry");
    assert_eq!(e.entry.source, CatalogSource::Listed);
    assert_eq!(e.entry.signed_by.as_deref(), Some(SRC));
    assert_eq!(e.entry.hash.as_deref(), Some(l.fn_hash.as_str()));
    assert_eq!(e.entry.label, l.label);
    assert_eq!(e.entry.description, l.description);
    let facets: Vec<(String, Vec<String>)> = e.facets.into_iter().collect();
    assert_eq!(
        facets,
        vec![
            ("domain".to_string(), vec!["finance".to_string()]),
            ("measure".to_string(), vec!["interest_rate".to_string()]),
            ("region".to_string(), vec!["gb".to_string()]),
            ("thing".to_string(), vec!["rate".to_string()]),
        ]
    );
    assert_eq!(s.listing_count().unwrap(), 1);
}

/// The facets of a listing in two subjects are each key's distinct values, ascending.
fn the_facets_of_two_subjects_are_the_distinct_values(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let mut l = listing(&bank_rate(), "a", T1);
    l.subjects = vec![SubjectKeys::new("finance", "rate", "interest_rate", "ea"), gb()];
    s.put_listing(&l).unwrap();
    let e = s.catalog_entry_ext(CatalogKind::Function, &bank_rate()).unwrap().unwrap();
    assert_eq!(e.facets.get("region"), Some(&vec!["ea".to_string(), "gb".to_string()]));
    assert_eq!(e.facets.get("domain"), Some(&vec!["finance".to_string()]));
    assert_eq!(s.listing(&bank_rate()).unwrap().unwrap().subjects, l.subjects);
}

/// L3 [25 Nov]: a run that finds nothing changed writes nothing: no row, no timestamp. The second put of the same listing
/// is `Unchanged` and leaves `fetched_at`, the catalogue entry's `updated_at` and every row count as they were.
fn an_unchanged_listing_writes_nothing(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listing(&listing(&bank_rate(), "a", T1)).unwrap();
    let before_at = updated_at(&db, &bank_rate());
    let counts = (untouched(&db), db.count("listings"), db.count("catalog_entries"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    let again = listing(&bank_rate(), "a", T2);
    assert_eq!(s.put_listing(&again).unwrap(), ListingWrite::Unchanged);
    let got = s.listing(&bank_rate()).unwrap().unwrap();
    assert_eq!(got.fetched_at, T1, "no timestamp is written by an unchanged put");
    assert_eq!(got.listed_at, T1);
    assert_eq!(updated_at(&db, &bank_rate()), before_at, "the catalogue entry is not rewritten");
    assert_eq!((untouched(&db), db.count("listings"), db.count("catalog_entries")), counts);
}

/// A changed listing (another descriptor) replaces the row: the new hash, descriptor and fetched_at; the first listed_at.
fn a_changed_listing_replaces_the_row_and_keeps_listed_at(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listing(&listing(&bank_rate(), "a", T1)).unwrap();
    let mut l2 = listing(&bank_rate(), "b", T3);
    l2.description = "Another description of the bank rate.".into();
    assert_eq!(s.put_listing(&l2).unwrap(), ListingWrite::Changed);
    let got = s.listing(&bank_rate()).unwrap().unwrap();
    assert_eq!(got.fn_hash, l2.fn_hash);
    assert_eq!(got.descriptor, l2.descriptor);
    assert_eq!(got.fetched_at, T3);
    assert_eq!(got.listed_at, T1, "listed_at is the first listing's");
    assert_eq!(s.listing_count().unwrap(), 1, "one row per name: no history");
    let e = s.catalog_entry_ext(CatalogKind::Function, &bank_rate()).unwrap().unwrap();
    assert_eq!(e.entry.hash.as_deref(), Some(l2.fn_hash.as_str()));
    assert_eq!(e.entry.description, l2.description);
    // both descriptors answer by hash (content-addressed)
    assert!(s.descriptor(&listing(&bank_rate(), "a", T1).fn_hash).is_some());
    assert!(s.descriptor(&l2.fn_hash).is_some());
    // a change of the subjects alone is a change too
    let mut l3 = l2.clone();
    l3.subjects = vec![SubjectKeys::new("finance", "rate", "interest_rate", "us")];
    assert_eq!(s.put_listing(&l3).unwrap(), ListingWrite::Changed);
    assert_eq!(s.listing(&bank_rate()).unwrap().unwrap().subjects, l3.subjects);
}

fn a_bad_listing_is_refused_and_nothing_is_written(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let good = listing(&bank_rate(), "a", T1);
    let mut mismatch = good.clone();
    mismatch.descriptor = desc("other");
    invalid(s.put_listing(&mismatch), "a descriptor that does not hash to fn_hash");
    let mut not_json = good.clone();
    not_json.descriptor = b"not a descriptor".to_vec();
    invalid(s.put_listing(&not_json), "a descriptor that does not parse");
    let mut elsewhere = good.clone();
    elsewhere.name = "third.example/bank_rate".into();
    invalid(s.put_listing(&elsewhere), "a name not under the host");
    let mut bad_host = good.clone();
    bad_host.host = "Not A Host".into();
    invalid(s.put_listing(&bad_host), "a host that is not a DNS name");
    let mut none = good.clone();
    none.subjects.clear();
    invalid(s.put_listing(&none), "no subject");
    let mut unsorted = good.clone();
    unsorted.subjects = vec![gb(), SubjectKeys::new("finance", "rate", "interest_rate", "ea")];
    invalid(s.put_listing(&unsorted), "subjects not ascending");
    let mut dup = good.clone();
    dup.subjects = vec![gb(), gb()];
    invalid(s.put_listing(&dup), "a subject twice");
    let mut many = good.clone();
    many.subjects =
        (0..=LISTING_SUBJECTS_MAX).map(|i| SubjectKeys::new("finance", "rate", "m", &format!("r{i:04}"))).collect();
    invalid(s.put_listing(&many), "more than 256 subjects");
    let mut bad_key = good.clone();
    bad_key.subjects = vec![SubjectKeys::new("Finance Rates", "rate", "interest_rate", "gb")];
    invalid(s.put_listing(&bad_key), "a key that is not a name");
    let mut long_key = good.clone();
    long_key.subjects = vec![SubjectKeys::new(&"a".repeat(33), "rate", "interest_rate", "gb")];
    invalid(s.put_listing(&long_key), "a key of more than 32 bytes");
    let mut no_desc = good.clone();
    no_desc.description.clear();
    invalid(s.put_listing(&no_desc), "an empty description");
    let mut no_key = good.clone();
    no_key.key_id.clear();
    invalid(s.put_listing(&no_key), "an empty key id");
    let mut bad_time = good.clone();
    bad_time.fetched_at = "yesterday".into();
    invalid(s.put_listing(&bad_time), "fetched_at not an instant");
    assert_eq!(db.count("listings"), 0);
    assert_eq!(db.count("catalog_entries"), 0);
    // exactly 256 subjects is accepted
    let mut max = good.clone();
    max.subjects =
        (0..LISTING_SUBJECTS_MAX).map(|i| SubjectKeys::new("finance", "rate", "m", &format!("r{i:04}"))).collect();
    assert_eq!(s.put_listing(&max).unwrap(), ListingWrite::Inserted);
}

fn listings_of_a_host_are_by_name_and_counted(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    for n in ["zeta", "alpha", "mid"] {
        s.put_listing(&listing(&format!("{SRC}/{n}"), n, T1)).unwrap();
    }
    let mut other = listing("other.example/alpha", "o", T1);
    other.host = "other.example".into();
    s.put_listing(&other).unwrap();
    let names: Vec<String> = s.listings_of(SRC).unwrap().into_iter().map(|l| l.name).collect();
    assert_eq!(names, [format!("{SRC}/alpha"), format!("{SRC}/mid"), format!("{SRC}/zeta")]);
    assert_eq!(s.listings_of("other.example").unwrap().len(), 1);
    assert_eq!(s.listings_of("nobody.example").unwrap().len(), 0);
    assert_eq!(s.listing_count().unwrap(), 4);
}

fn a_deleted_listing_takes_its_entry_and_keeps_its_descriptor(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let l = listing(&bank_rate(), "a", T1);
    s.put_listing(&l).unwrap();
    assert!(s.delete_listing(&bank_rate()).unwrap());
    assert!(!s.delete_listing(&bank_rate()).unwrap(), "nothing left to delete");
    assert_eq!(s.listing(&bank_rate()).unwrap(), None);
    assert_eq!(s.catalog_entry_ext(CatalogKind::Function, &bank_rate()).unwrap(), None);
    assert_eq!(s.listing_count().unwrap(), 0);
    assert!(s.descriptor(&l.fn_hash).is_some(), "descriptors are content-addressed and stay");
    // listed again later: a new row
    assert_eq!(s.put_listing(&listing(&bank_rate(), "a", T3)).unwrap(), ListingWrite::Inserted);
    assert_eq!(s.listing(&bank_rate()).unwrap().unwrap().listed_at, T3);
}

/// L3: a listing is not a binding. No name binding, no namespace, no cell, receipt or version row is written; the name does
/// not resolve; the host has no bindings.
fn a_listing_is_not_a_binding(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let before = untouched(&db);
    let l = listing(&bank_rate(), "a", T1);
    s.put_listing(&l).unwrap();
    let mut after = untouched(&db);
    // the descriptor is the one row the listing adds outside its own tables and the catalogue
    assert_eq!(after[0], before[0] + 1, "one descriptor row");
    after[0] = before[0];
    assert_eq!(after, before, "receipts, versions, cells, name_bindings, namespaces, class_bindings: unchanged");
    assert_eq!(s.resolve_name(&bank_rate(), "9999-12-31T23:59:59.999999Z").unwrap(), None);
    assert!(!s.has_bindings_under(SRC).unwrap());
    assert_eq!(s.namespace(SRC).unwrap(), None);
}

// ---- listed hosts --------------------------------------------------------------------------------------------------

fn a_listed_host_row_is_written_once_per_change(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    assert_eq!(s.listed_host(SRC).unwrap(), None);
    let seen = ListedHost { host: SRC.into(), last_seen: Some("2026-11-16".into()), unreachable_since: None };
    assert!(s.put_listed_host(&seen).unwrap());
    assert!(!s.put_listed_host(&seen).unwrap(), "the same row: nothing written");
    assert_eq!(s.listed_host(SRC).unwrap(), Some(seen.clone()));
    let down = ListedHost { unreachable_since: Some(T2.into()), ..seen.clone() };
    assert!(s.put_listed_host(&down).unwrap());
    assert_eq!(s.listed_host(SRC).unwrap(), Some(down));
    let other = ListedHost { host: "a.example".into(), last_seen: None, unreachable_since: None };
    assert!(s.put_listed_host(&other).unwrap());
    let hosts: Vec<String> = s.listed_hosts().unwrap().into_iter().map(|h| h.host).collect();
    assert_eq!(hosts, ["a.example".to_string(), SRC.to_string()]);
    assert_eq!(db.count("listed_hosts"), 2);
}

fn a_bad_listed_host_is_refused(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    for (h, what) in [
        (ListedHost { host: "Not A Host".into(), last_seen: None, unreachable_since: None }, "host"),
        (ListedHost { host: SRC.into(), last_seen: Some("16 Nov".into()), unreachable_since: None }, "last_seen a day"),
        (
            ListedHost { host: SRC.into(), last_seen: Some(T1.into()), unreachable_since: None },
            "last_seen a day, not an instant",
        ),
        (ListedHost { host: SRC.into(), last_seen: None, unreachable_since: Some("2026-11-16".into()) }, "an instant"),
    ] {
        match s.put_listed_host(&h) {
            Err(Error::Invalid(_)) => {}
            other => panic!("{what}: want Error::Invalid, got {other:?}"),
        }
    }
    assert_eq!(db.count("listed_hosts"), 0);
}

fn deleting_a_listed_host_deletes_its_listings(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listed_host(&ListedHost { host: SRC.into(), last_seen: Some("2026-11-16".into()), unreachable_since: None })
        .unwrap();
    s.put_listing(&listing(&bank_rate(), "a", T1)).unwrap();
    s.put_listing(&listing(&format!("{SRC}/other"), "b", T1)).unwrap();
    let mut keep = listing("keep.example/rate", "k", T1);
    keep.host = "keep.example".into();
    s.put_listing(&keep).unwrap();
    assert!(s.delete_listed_host(SRC).unwrap());
    assert_eq!(s.listed_host(SRC).unwrap(), None);
    assert_eq!(s.listings_of(SRC).unwrap().len(), 0);
    assert_eq!(s.catalog_entry_ext(CatalogKind::Function, &bank_rate()).unwrap(), None);
    assert_eq!(s.listing_count().unwrap(), 1, "another host's listing stays");
    assert!(!s.delete_listed_host(SRC).unwrap());
}

backends!(
    migration_18_and_14_are_the_last_and_named,
    the_tables_have_the_contracts_columns,
    the_migration_changes_no_stored_row,
    the_catalogue_source_takes_listed,
    a_listing_is_written_with_its_descriptor_and_catalogue_entry,
    the_facets_of_two_subjects_are_the_distinct_values,
    an_unchanged_listing_writes_nothing,
    a_changed_listing_replaces_the_row_and_keeps_listed_at,
    a_bad_listing_is_refused_and_nothing_is_written,
    listings_of_a_host_are_by_name_and_counted,
    a_deleted_listing_takes_its_entry_and_keeps_its_descriptor,
    a_listing_is_not_a_binding,
    a_listed_host_row_is_written_once_per_change,
    a_bad_listed_host_is_refused,
    deleting_a_listed_host_deletes_its_listings,
);
