//! Store 0.7.2 (patch), the listing re-review's N1 (decided 27 Nov in CONTRACT-http-listing.md, "the re-review"): a
//! listed name's own 7-day clock. Migration 19 (PostgreSQL) / 15 (SQLite) adds `listings.unreached_since` (nullable, an
//! instant); `Listing` is unchanged. Two `Store` methods with defaults:
//!
//! ```text
//! fn set_listing_unreached(&self, name: &str, since: Option<&str>) -> Result<bool>;   // false: equal, nothing written
//! fn listing_unreached(&self, name: &str) -> Result<Option<String>>;
//! ```
//!
//! Both backends. Failing on the test commit (the methods are `Unsupported`, there is no migration 19 / 15).

#[macro_use]
mod common;

use common::*;
use datumesh_core::Type;
use datumesh_store::{migrate, Error, Listing, ListingWrite, SubjectKeys};

const SRC: &str = "source.example";
const T1: &str = "2026-11-16T00:00:00.000000Z";
const T2: &str = "2026-11-17T00:00:00.000000Z";
const T3: &str = "2026-11-18T00:00:00.000000Z";

fn listing(name: &str, body: &str, at: &str) -> Listing {
    let d = stored_descriptor(&[("date", Type::Date)], body);
    Listing {
        host: SRC.to_string(),
        name: name.to_string(),
        fn_hash: fn_hash_of(&d),
        subjects: vec![SubjectKeys::new("finance", "rate", "interest_rate", "gb")],
        descriptor: d,
        key_id: format!("{SRC}#k1"),
        label: "Bank Rate".to_string(),
        description: "The bank rate of the source.".to_string(),
        fetched_at: at.to_string(),
        listed_at: at.to_string(),
    }
}

fn name() -> String {
    format!("{SRC}/bank_rate")
}

fn listing_row(db: &TestDb) -> Vec<Vec<String>> {
    db.texts(&format!(
        "SELECT name || '|' || fn_hash || '|' || fetched_at || '|' || listed_at || '|' || COALESCE(unreached_since, '') \
         FROM listings WHERE name = '{}'",
        name()
    ))
}

fn migration_19_and_15_are_the_last_and_named(b: Backend) {
    let ms = match b {
        Backend::Pg => migrate::MIGRATIONS,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS,
    };
    let last = ms.last().unwrap();
    let want = if b == Backend::Pg { (19, "0019_listing_unreached.sql") } else { (15, "0015_listing_unreached.sql") };
    assert_eq!((last.version, last.name), want);
    assert_eq!(ms.len() as u32, want.0);
}

/// The column is nullable, and existing listings get none (the migration is additive: no other column changes).
fn the_migration_adds_a_nullable_column_to_existing_rows(b: Backend) {
    let db = TestDb::empty(b);
    let n_old = if b == Backend::Pg { 18 } else { 14 };
    let old: Vec<&migrate::Migration> = match b {
        Backend::Pg => migrate::MIGRATIONS[..n_old].iter().collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS[..n_old].iter().collect(),
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
    db.bare_store().put_listing(&listing(&name(), "a", T1)).unwrap();
    let before = db.texts(&format!(
        "SELECT name || '|' || fn_hash || '|' || fetched_at || '|' || listed_at FROM listings WHERE name = '{}'",
        name()
    ));
    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(datumesh_store::SqliteOptions::new())).unwrap(),
    };
    assert_eq!(applied, vec![n_old as u32 + 1]);
    assert_eq!(listing_row(&db), vec![vec![format!("{}|{}", before[0][0], "")]], "the row unchanged, no clock");
    assert_eq!(db.bare_store().listing_unreached(&name()).unwrap(), None);
}

fn the_clock_is_set_cleared_and_written_only_on_a_change(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listing(&listing(&name(), "a", T1)).unwrap();
    assert_eq!(s.listing_unreached(&name()).unwrap(), None);
    assert!(s.set_listing_unreached(&name(), Some(T2)).unwrap(), "set");
    assert_eq!(s.listing_unreached(&name()).unwrap().as_deref(), Some(T2));
    let row = listing_row(&db);
    assert!(!s.set_listing_unreached(&name(), Some(T2)).unwrap(), "the same: nothing written");
    // another instant is a change (the caller keeps the first; the store writes what it is given)
    assert!(s.set_listing_unreached(&name(), Some(T3)).unwrap());
    assert_eq!(s.listing_unreached(&name()).unwrap().as_deref(), Some(T3));
    assert!(s.set_listing_unreached(&name(), Some(T2)).unwrap());
    assert_eq!(listing_row(&db), row, "only the clock column, back to T2");
    assert!(s.set_listing_unreached(&name(), None).unwrap(), "cleared");
    assert!(!s.set_listing_unreached(&name(), None).unwrap(), "already clear: nothing written");
    assert_eq!(s.listing_unreached(&name()).unwrap(), None);
    let got = s.listing(&name()).unwrap().unwrap();
    assert_eq!((got.fetched_at.as_str(), got.listed_at.as_str()), (T1, T1), "fetched_at untouched");
}

/// An unchanged put leaves the clock; a changed put leaves it too (the listing's run decides); a delete takes it, and a new
/// listing of the name starts with none.
fn the_clock_lives_with_its_listing(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listing(&listing(&name(), "a", T1)).unwrap();
    s.set_listing_unreached(&name(), Some(T2)).unwrap();
    assert_eq!(s.put_listing(&listing(&name(), "a", T3)).unwrap(), ListingWrite::Unchanged);
    assert_eq!(s.listing_unreached(&name()).unwrap().as_deref(), Some(T2));
    assert_eq!(s.put_listing(&listing(&name(), "b", T3)).unwrap(), ListingWrite::Changed);
    assert_eq!(s.listing_unreached(&name()).unwrap().as_deref(), Some(T2));
    assert!(s.delete_listing(&name()).unwrap());
    assert_eq!(s.listing_unreached(&name()).unwrap(), None);
    s.put_listing(&listing(&name(), "a", T3)).unwrap();
    assert_eq!(s.listing_unreached(&name()).unwrap(), None, "a new listing has no clock");
}

fn bad_input_is_refused(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    match s.set_listing_unreached(&name(), Some(T1)) {
        Err(Error::NotFound { .. }) => {}
        other => panic!("no listing: want NotFound, got {other:?}"),
    }
    s.put_listing(&listing(&name(), "a", T1)).unwrap();
    match s.set_listing_unreached(&name(), Some("2026-11-17")) {
        Err(Error::Invalid(_)) => {}
        other => panic!("not an instant: want Invalid, got {other:?}"),
    }
    assert_eq!(s.listing_unreached(&name()).unwrap(), None, "nothing written");
    assert_eq!(s.listing_unreached("other.example/x").unwrap(), None, "no listing: none");
}

backends!(
    migration_19_and_15_are_the_last_and_named,
    the_migration_adds_a_nullable_column_to_existing_rows,
    the_clock_is_set_cleared_and_written_only_on_a_change,
    the_clock_lives_with_its_listing,
    bad_input_is_refused,
);
