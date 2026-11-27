//! Store 0.7.1 (patch), the listing review round (REVIEW-listing-060.md C3 and B4, decided 27 Nov in
//! CONTRACT-http-listing.md, "the review's open points"). Both backends. Failing on 0.7.0's code; no API change.
//!
//! * C3: a listing never overwrites (and so never later deletes) a catalogue entry of another source. `put_listing` over a
//!   function entry of source `publisher` or `operator` of the same name is `Error::Invalid` ("a catalogue entry of another
//!   source has this name"), and nothing is written: the entry stays byte for byte, no listing row, no descriptor row.
//! * B4: when a listing drops (`delete_listing`, `delete_listed_host`), its descriptor is removed if nothing else
//!   references it: no name binding, no other listing, no stored cell of that function hash.

#[macro_use]
mod common;

use common::*;
use datumesh_core::Type;
use datumesh_store::{CatalogEntry, CatalogEntryExt, CatalogKind, CatalogSource, Error, ListedHost, Listing};

const SRC: &str = "source.example";
const T1: &str = "2026-11-16T00:00:00.000000Z";

fn desc(body: &str) -> Vec<u8> {
    stored_descriptor(&[("date", Type::Date)], body)
}

fn listing(name: &str, body: &str) -> Listing {
    let d = desc(body);
    Listing {
        host: SRC.to_string(),
        name: name.to_string(),
        fn_hash: fn_hash_of(&d),
        subjects: vec![datumesh_store::SubjectKeys::new("finance", "rate", "interest_rate", "gb")],
        descriptor: d,
        key_id: format!("{SRC}#k1"),
        label: "Bank Rate".to_string(),
        description: "The bank rate of the source.".to_string(),
        fetched_at: T1.to_string(),
        listed_at: T1.to_string(),
    }
}

fn entry(name: &str, source: CatalogSource) -> CatalogEntryExt {
    let mut args = std::collections::BTreeMap::new();
    args.insert("date".to_string(), "The day.".to_string());
    let mut e = CatalogEntryExt::new(CatalogEntry {
        kind: CatalogKind::Function,
        name: name.to_string(),
        hash: None,
        label: "Published here".into(),
        description: "The publisher's own entry.".into(),
        args,
        examples: "[]".into(),
        source,
        signed_by: Some("source.example#k0".into()),
    });
    e.facets.insert("domain".into(), vec!["finance".into()]);
    e.source_name = Some("The source".into());
    e
}

fn entry_rows(db: &TestDb, name: &str) -> Vec<Vec<String>> {
    db.texts(&format!(
        "SELECT kind || '|' || name || '|' || source || '|' || label || '|' || description || '|' || args || '|' || \
         COALESCE(source_name, '') || '|' || CAST(updated_at AS TEXT) FROM catalog_entries WHERE name = '{name}'"
    ))
}

// ---- C3 --------------------------------------------------------------------------------------------------------------------

fn a_listing_never_overwrites_another_sources_entry(b: Backend) {
    for source in [CatalogSource::Publisher, CatalogSource::Operator] {
        let db = TestDb::migrated(b);
        let s = db.bare_store();
        let name = format!("{SRC}/bank_rate");
        s.upsert_catalog_ext(&[entry(&name, source)]).unwrap();
        let before = (entry_rows(&db, &name), db.count("descriptors"), db.count("listings"));
        match s.put_listing(&listing(&name, "a")) {
            Err(Error::Invalid(_)) => {}
            other => panic!("{source:?}: want Error::Invalid, got {other:?}"),
        }
        assert_eq!(
            (entry_rows(&db, &name), db.count("descriptors"), db.count("listings")),
            before,
            "{source:?}: nothing written"
        );
        assert_eq!(s.listing(&name).unwrap(), None);
        // and so a delete of the (absent) listing never deletes the entry
        assert!(!s.delete_listing(&name).unwrap());
        let e = s.catalog_entry_ext(CatalogKind::Function, &name).unwrap().expect("the entry stays");
        assert_eq!(e.entry.source, source);
        assert_eq!(e.source_name.as_deref(), Some("The source"));
    }
}

/// A listed entry is still replaced by the next put of its listing (the guard is on the stored row's source only).
fn a_listing_still_replaces_its_own_entry(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let name = format!("{SRC}/bank_rate");
    s.put_listing(&listing(&name, "a")).unwrap();
    let mut l2 = listing(&name, "b");
    l2.description = "Another description.".into();
    assert_eq!(s.put_listing(&l2).unwrap(), datumesh_store::ListingWrite::Changed);
    let e = s.catalog_entry_ext(CatalogKind::Function, &name).unwrap().unwrap();
    assert_eq!((e.entry.source, e.entry.description.as_str()), (CatalogSource::Listed, "Another description."));
}

// ---- B4: descriptors of dropped listings -----------------------------------------------------------------------------------

fn an_unreferenced_descriptor_goes_with_its_listing(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let l = listing(&format!("{SRC}/bank_rate"), "a");
    let before = db.count("descriptors");
    s.put_listing(&l).unwrap();
    assert_eq!(db.count("descriptors"), before + 1);
    assert!(s.delete_listing(&l.name).unwrap());
    assert!(s.descriptor(&l.fn_hash).is_none(), "removed: nothing references it");
    assert_eq!(db.count("descriptors"), before);
}

fn a_descriptor_shared_by_another_listing_stays(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let one = listing(&format!("{SRC}/bank_rate"), "same");
    let two = listing(&format!("{SRC}/bank_rate_copy"), "same");
    assert_eq!(one.fn_hash, two.fn_hash);
    s.put_listing(&one).unwrap();
    s.put_listing(&two).unwrap();
    s.delete_listing(&one.name).unwrap();
    assert!(s.descriptor(&one.fn_hash).is_some(), "the other listing uses it");
    s.delete_listing(&two.name).unwrap();
    assert!(s.descriptor(&one.fn_hash).is_none(), "now nothing does");
}

fn a_descriptor_bound_here_stays(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let l = listing(&format!("{SRC}/bank_rate"), "bound");
    let h = s.put_descriptor(&l.descriptor).unwrap();
    assert_eq!(h, l.fn_hash);
    s.bind_name(&format!("{SLICE_HOST}/local"), &h, VALIDATED_AT).unwrap();
    s.put_listing(&l).unwrap();
    s.delete_listing(&l.name).unwrap();
    assert!(s.descriptor(&h).is_some(), "a binding uses it");
}

fn a_descriptor_with_stored_cells_stays(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    let d = stored_descriptor(&[("k", Type::Number)], "cells");
    let h = s.put_descriptor(&d).unwrap();
    put_member(&s, &slice_cell("f", &h, &[("k", sc(Type::Number, "1"))]), "1", T1, T1);
    let mut l = listing(&format!("{SRC}/bank_rate"), "x");
    l.descriptor = d;
    l.fn_hash = h.clone();
    s.put_listing(&l).unwrap();
    s.delete_listing(&l.name).unwrap();
    assert!(s.descriptor(&h).is_some(), "a stored cell of the function uses it");
}

fn deleting_a_listed_host_removes_its_unreferenced_descriptors(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_listed_host(&ListedHost { host: SRC.into(), last_seen: Some("2026-11-16".into()), unreachable_since: None })
        .unwrap();
    let a = listing(&format!("{SRC}/a_rate"), "a");
    let bb = listing(&format!("{SRC}/b_rate"), "b");
    s.put_listing(&a).unwrap();
    s.put_listing(&bb).unwrap();
    assert!(s.delete_listed_host(SRC).unwrap());
    assert!(s.descriptor(&a.fn_hash).is_none() && s.descriptor(&bb.fn_hash).is_none());
}

backends!(
    a_listing_never_overwrites_another_sources_entry,
    a_listing_still_replaces_its_own_entry,
    an_unreferenced_descriptor_goes_with_its_listing,
    a_descriptor_shared_by_another_listing_stays,
    a_descriptor_bound_here_stays,
    a_descriptor_with_stored_cells_stays,
    deleting_a_listed_host_removes_its_unreferenced_descriptors,
);
