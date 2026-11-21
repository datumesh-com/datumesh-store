//! Store 0.5.2, S7 and S8 (CONTRACT-0.4.9): the exact dictionary lookups (`has_key_value`,
//! `key_value_count`, `mark_key_open`, `key_is_open`) and the exact catalogue lookup
//! (`catalog_entry`). Both backends.

#[macro_use]
mod common;

use std::collections::BTreeMap;

use common::*;
use datumesh_store::{CatalogEntry, CatalogKind, CatalogSource, KeyValue};

const OWNER: &str = "scripture.example.org/verse";
const OWNER2: &str = "scripture.example.org/chapter";

fn kv(value: &str, label: &str) -> KeyValue {
    KeyValue { value: value.to_string(), label: label.to_string(), aliases: vec![] }
}

fn function(name: &str, label: &str) -> CatalogEntry {
    CatalogEntry {
        kind: CatalogKind::Function,
        name: name.to_string(),
        hash: None,
        label: label.to_string(),
        description: format!("{label}, in words."),
        args: BTreeMap::new(),
        examples: "[]".to_string(),
        source: CatalogSource::Operator,
        signed_by: None,
    }
}

fn has_key_value_is_exact(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis"), kv("Genesis", ""), kv("2Chr", "2 Chronicles")]).unwrap();
    s.upsert_key_values(OWNER2, "book", &[kv("Exo", "Exodus")]).unwrap();
    assert!(s.has_key_value(OWNER, "book", "Gen").unwrap());
    assert!(s.has_key_value(OWNER, "book", "Genesis").unwrap());
    assert!(s.has_key_value(OWNER, "book", "2Chr").unwrap());
    // Case-sensitive.
    assert!(!s.has_key_value(OWNER, "book", "gen").unwrap());
    assert!(!s.has_key_value(OWNER, "book", "GEN").unwrap());
    // No substring or prefix match, in either direction; the label is not a value.
    assert!(!s.has_key_value(OWNER, "book", "Ge").unwrap());
    assert!(!s.has_key_value(OWNER, "book", "enesis").unwrap());
    assert!(!s.has_key_value(OWNER, "book", "Genesis1").unwrap());
    assert!(!s.has_key_value(OWNER, "book", "2 Chronicles").unwrap());
    // Wildcard characters are literal.
    assert!(!s.has_key_value(OWNER, "book", "Ge%").unwrap());
    assert!(!s.has_key_value(OWNER, "book", "G_n").unwrap());
    // Another owner or key: not there.
    assert!(!s.has_key_value(OWNER, "chapter", "Gen").unwrap());
    assert!(!s.has_key_value(OWNER2, "book", "Gen").unwrap());
    assert!(s.has_key_value(OWNER2, "book", "Exo").unwrap());
    // An owner with nothing.
    assert!(!s.has_key_value("scripture.example.org/none", "book", "Gen").unwrap());
}

fn key_value_count_counts_the_values_of_an_owners_key(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    assert_eq!(s.key_value_count(OWNER, "book").unwrap(), 0);
    s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis"), kv("Exo", "Exodus"), kv("Lev", "")]).unwrap();
    s.upsert_key_values(OWNER, "chapter", &[kv("1", "")]).unwrap();
    s.upsert_key_values(OWNER2, "book", &[kv("Gen", "")]).unwrap();
    assert_eq!(s.key_value_count(OWNER, "book").unwrap(), 3);
    assert_eq!(s.key_value_count(OWNER, "chapter").unwrap(), 1);
    assert_eq!(s.key_value_count(OWNER2, "book").unwrap(), 1);
    assert_eq!(s.key_value_count(OWNER, "other").unwrap(), 0);
    // An upsert of an existing value is not a new value.
    s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis, again"), kv("Num", "")]).unwrap();
    assert_eq!(s.key_value_count(OWNER, "book").unwrap(), 4);
}

fn mark_key_open_is_idempotent_and_key_is_open_reads_it(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis")]).unwrap();
    assert!(!s.key_is_open(OWNER, "book").unwrap());
    s.mark_key_open(OWNER, "book").unwrap();
    assert!(s.key_is_open(OWNER, "book").unwrap());
    // Again: no error, one row, still open.
    s.mark_key_open(OWNER, "book").unwrap();
    s.mark_key_open(OWNER, "book").unwrap();
    assert!(s.key_is_open(OWNER, "book").unwrap());
    assert_eq!(db.count("open_keys"), 1);
    // Only that (owner, key).
    assert!(!s.key_is_open(OWNER, "chapter").unwrap());
    assert!(!s.key_is_open(OWNER2, "book").unwrap());
    s.mark_key_open(OWNER2, "book").unwrap();
    assert_eq!(db.count("open_keys"), 2);
    // Existing values stay: nothing is deleted.
    assert!(s.has_key_value(OWNER, "book", "Gen").unwrap());
    assert_eq!(s.key_value_count(OWNER, "book").unwrap(), 1);
    // A key can be marked before it has any value.
    s.mark_key_open(OWNER, "accession").unwrap();
    assert!(s.key_is_open(OWNER, "accession").unwrap());
    assert_eq!(s.key_value_count(OWNER, "accession").unwrap(), 0);
}

fn catalog_entry_is_an_exact_lookup_by_name(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let a = function("scripture.example.org/verse", "Verse");
    let c = function("scripture.example.org/verse_count", "Verse count");
    let d = function("other.example.org/verse", "Other verse");
    s.upsert_catalog(&[a.clone(), c.clone(), d.clone()]).unwrap();
    assert_eq!(s.catalog_entry("scripture.example.org/verse").unwrap(), Some(a));
    assert_eq!(s.catalog_entry("scripture.example.org/verse_count").unwrap(), Some(c));
    assert_eq!(s.catalog_entry("other.example.org/verse").unwrap(), Some(d));
    // Not a prefix, a substring, another case, or a wildcard.
    assert_eq!(s.catalog_entry("scripture.example.org/vers").unwrap(), None);
    assert_eq!(s.catalog_entry("example.org/verse").unwrap(), None);
    assert_eq!(s.catalog_entry("scripture.example.org/Verse").unwrap(), None);
    assert_eq!(s.catalog_entry("scripture.example.org/verse%").unwrap(), None);
    assert_eq!(s.catalog_entry("scripture.example.org/none").unwrap(), None);
}

fn open_key_count_counts_the_open_keys_of_every_owner(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    assert_eq!(s.open_key_count().unwrap(), 0);
    s.mark_key_open(OWNER, "book").unwrap();
    s.mark_key_open(OWNER2, "book").unwrap();
    assert_eq!(s.open_key_count().unwrap(), 2);
    // Marking an open key again keeps one row.
    s.mark_key_open(OWNER, "book").unwrap();
    assert_eq!(s.open_key_count().unwrap(), 2);
}

backends!(
    open_key_count_counts_the_open_keys_of_every_owner,
    has_key_value_is_exact,
    key_value_count_counts_the_values_of_an_owners_key,
    mark_key_open_is_idempotent_and_key_is_open_reads_it,
    catalog_entry_is_an_exact_lookup_by_name,
);
