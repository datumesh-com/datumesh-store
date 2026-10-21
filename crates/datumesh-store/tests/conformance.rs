//! Conformance: the core's resolver test kit passes for `PgStore` (time-aware), and every
//! receipt vector the core accepts round-trips through the store.
//!
//! The store keeps the canonical form (JCS) of what it is given, never the bytes as given:
//! the vectors' inputs are deliberately non-canonical JSON text, and the receipt hash does
//! not cover `sig` or the JSON layout, so "bytes as given" would make the stored bytes depend
//! on which of several equivalent texts arrived first. `receipt(hash)` therefore returns the
//! JCS of the input: byte for byte the input when the input is canonical, and always text that
//! `Receipt::from_json` accepts and that hashes to `hash`.

mod common;

use common::*;
use datumesh_core::testkit::{Fixtures, Kit};
use datumesh_core::{Receipt, Resolver};
use datumesh_store::{PgStore, Store};

fn load(db: &TestDb, fx: &Fixtures) -> PgStore {
    let s = db.store();
    for (h, b) in &fx.descriptors {
        assert_eq!(&s.put_descriptor(b).unwrap(), h);
    }
    for (h, b) in &fx.receipts {
        assert_eq!(&s.put_receipt(b).unwrap(), h);
    }
    for v in &fx.versions {
        s.put_version(&v.cell_id, &v.tx, &v.receipt_hash).unwrap();
    }
    s
}

#[test]
fn testkit_passes_time_aware() {
    let db = TestDb::migrated();
    if let Err(report) = Kit::new().time_aware(true).run(|fx| load(&db, fx)) {
        panic!("{report}");
    }
}

#[test]
fn testkit_passes_through_connect_url() {
    // The same, with PgStore::connect and a pool of one connection.
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    {
        let s = db.store();
        let fx = kit.fixtures();
        for (_, b) in &fx.descriptors {
            s.put_descriptor(b).unwrap();
        }
        for (_, b) in &fx.receipts {
            s.put_receipt(b).unwrap();
        }
        for v in &fx.versions {
            s.put_version(&v.cell_id, &v.tx, &v.receipt_hash).unwrap();
        }
    }
    let s = PgStore::connect(&db.url()).unwrap();
    if let Err(report) = kit.check(&s) {
        panic!("{report}");
    }
}

#[test]
fn every_accepted_receipt_vector_round_trips() {
    let db = TestDb::migrated();
    let s = db.store();
    let mut groups = vectors("receipt");
    groups.extend(vectors("invalid").into_iter().filter(|v| str_of(v, "kind") == "receipt"));
    let mut seen = 0;
    for v in groups {
        let id = str_of(&v, "id");
        let bytes = input(&v);
        let Ok(parsed) = Receipt::from_json(&bytes) else { continue };
        seen += 1;
        // Some invalid vectors carry the payload of a valid one with another signature: the
        // same hash. The store refuses the second (Conflict); round-trip it in a database
        // of its own.
        let own;
        let s = match s.put_receipt(&bytes) {
            Err(datumesh_store::Error::Conflict { hash }) => {
                assert_eq!(hash, parsed.hash(), "{id}");
                own = TestDb::migrated();
                own.store()
            }
            Err(e) => panic!("{id}: {e}"),
            Ok(_) => db.store(),
        };
        let h = s.put_receipt(&bytes).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(h, parsed.hash(), "{id}");
        let back = s.receipt(&h).unwrap_or_else(|| panic!("{id}: not stored"));
        // Canonical form: the JCS of the input, which parses and hashes back to h.
        assert_eq!(&*back, jcs(&bytes).as_slice(), "{id}");
        let again = Receipt::from_json(&back).unwrap();
        assert_eq!(again.hash(), h, "{id}");
        assert_eq!(again, parsed, "{id}");
        // Given its canonical form, the store returns exactly those bytes.
        assert_eq!(s.put_receipt(&back).unwrap(), h, "{id}");
        assert_eq!(&*s.receipt(&h).unwrap(), &*back, "{id}");
    }
    assert!(seen >= 38, "{seen}");
}

#[test]
fn stored_vectors_recompute_through_the_store() {
    // Load the descriptor, receipt and version of every vector whose recompute object names
    // a stored function and whose receipt verifies: verify through the store reaches
    // recomputed (blob content given for blob values).
    let mut seen = 0;
    for v in vectors("receipt") {
        let Some(rc) = v.get("recompute") else { continue };
        let Some(desc) = rc.get("descriptor") else { continue };
        if desc.get("code").and_then(|c| c.get("lang")).and_then(|l| l.as_str()) != Some("stored") {
            continue;
        }
        let id = str_of(&v, "id");
        let db = TestDb::migrated();
        let s = db.store();
        s.put_descriptor(&desc.to_jcs()).unwrap();
        let bytes = input(&v);
        let h = s.put_receipt(&bytes).unwrap();
        let r = Receipt::from_json(&bytes).unwrap();
        s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
        let blob = v.get("blob_hex").and_then(|b| b.as_str()).map(hex);
        if let Some(content) = &blob {
            let sha = s.put_blob(content).unwrap();
            assert_eq!(s.blob(&sha).as_deref(), Some(content.as_slice()));
        }
        let out = datumesh_core::verify(&bytes, &keys(&v), Some(&s as &dyn Resolver), blob.as_deref());
        assert!(out.ok, "{id}: {out:?}");
        assert_eq!(out.level, Some(datumesh_core::Level::Recomputed), "{id}: {out:?}");
        seen += 1;
    }
    assert!(seen >= 2, "{seen}");
}
