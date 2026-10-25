//! Conformance: the core's resolver test kit passes for `PgStore` (time-aware, with the
//! kit's default answer rule `AnswerRule::TxAndAsof`: since 0.3.0 `evaluate` answers as
//! `stored_asof` does, so a version answers as of A only when its tx and its receipt's asof
//! are both at or before A; since 0.3.1 with the slice checks, `Kit::slices(true)`: the
//! store serves manifests); every
//! receipt vector is stored exactly when it verifies (steps 1-6) against its key documents,
//! refused with verify's code otherwise, and round-trips through the store.
//!
//! The store keeps the canonical form (JCS) of what it is given, never the bytes as given:
//! the vectors' inputs are deliberately non-canonical JSON text, and the receipt hash does
//! not cover `sig` or the JSON layout, so "bytes as given" would make the stored bytes depend
//! on which of several equivalent texts arrived first. `receipt(hash)` therefore returns the
//! JCS of the input: byte for byte the input when the input is canonical, and always text that
//! `Receipt::from_json` accepts and that hashes to `hash`.

mod common;

use common::*;
use datumesh_core::testkit::{AnswerRule, Fixtures, Kit};
use datumesh_core::{ErrorCode, Receipt, Resolver};
use datumesh_store::{PgStore, Store};

fn load(db: &TestDb, fx: &Fixtures) -> PgStore {
    let s = db.bare_store();
    // Key documents first: receipts are verified against them.
    for (host, doc) in &fx.key_documents {
        s.put_namespace(host, doc, VALIDATED_AT).unwrap();
    }
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

/// `load`, and the slice fixtures (0.3.1): their key documents, descriptors (formula
/// descriptors with `signature.order`), receipts (citing receipts and members) and manifests.
fn load_with_slices(db: &TestDb, fx: &Fixtures) -> PgStore {
    let s = load(db, fx);
    let sf = &fx.slices;
    for (host, doc) in &sf.key_documents {
        s.put_namespace(host, doc, VALIDATED_AT).unwrap();
    }
    for (h, b) in &sf.descriptors {
        assert_eq!(&s.put_descriptor(b).unwrap(), h);
    }
    for (h, b) in &sf.receipts {
        assert_eq!(&s.put_receipt(b).unwrap(), h);
    }
    for (e, b) in &sf.manifests {
        assert_eq!(&s.put_manifest(b).unwrap(), e);
    }
    s
}

#[test]
fn testkit_passes_time_aware_with_slices() {
    // Time-aware, TxAndAsof (the default), and the slice checks: manifests by entry, exactly
    // as loaded, members from the store, None for every entry not loaded.
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true).slices(true);
    assert!(!kit.fixtures().slices.manifests.is_empty());
    if let Err(report) = kit.run(|fx| load_with_slices(&db, fx)) {
        panic!("{report}");
    }
}

#[test]
fn testkit_slice_checks_fail_without_the_manifests() {
    // Loaded as for 0.3.0 (no slice fixtures): the kit passes without the slice checks and
    // fails with them.
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    let s = load(&db, kit.fixtures());
    assert!(kit.check(&s).is_ok());
    assert!(Kit::new().time_aware(true).slices(true).check(&s).is_err());
}

#[test]
fn testkit_passes_time_aware() {
    // The kit's default answer rule, TxAndAsof (datumesh-core 0.3.3).
    let db = TestDb::migrated();
    if let Err(report) = Kit::new().time_aware(true).run(|fx| load(&db, fx)) {
        panic!("{report}");
    }
}

#[test]
fn testkit_passes_with_tx_and_asof_and_fails_with_tx_only() {
    // Named explicitly: the store answers with both times. Under the old tx-only rule the
    // kit expects the EUR/CHF cell's first version from its tx on, before its receipt's
    // asof, which the store does not answer: that rule fails.
    let db = TestDb::migrated();
    if let Err(report) = Kit::new().time_aware(true).answer_rule(AnswerRule::TxAndAsof).run(|fx| load(&db, fx)) {
        panic!("{report}");
    }
    let db = TestDb::migrated();
    assert!(Kit::new().time_aware(true).answer_rule(AnswerRule::TxOnly).run(|fx| load(&db, fx)).is_err());
}

#[test]
fn testkit_passes_through_connect_url() {
    // The same, with PgStore::connect and a pool of one connection.
    let db = TestDb::migrated();
    let kit = Kit::new().time_aware(true);
    {
        let s = db.bare_store();
        let fx = kit.fixtures();
        for (host, doc) in &fx.key_documents {
            s.put_namespace(host, doc, VALIDATED_AT).unwrap();
        }
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
fn every_receipt_vector_is_stored_exactly_when_it_verifies() {
    let mut groups = vectors("receipt");
    groups.extend(vectors("invalid").into_iter().filter(|v| str_of(v, "kind") == "receipt"));
    let (mut stored, mut refused) = (0, 0);
    for v in groups {
        let id = str_of(&v, "id");
        let bytes = input(&v);
        // A database of its own, with this vector's key documents (those a 200 response
        // offers and that satisfy SPEC 9.2).
        let db = TestDb::migrated();
        let s = db.bare_store();
        let mut rejected_doc = false;
        for (host, env) in v.get("keys").and_then(|k| k.as_object()).unwrap_or(&[]) {
            let https = env.get("https").unwrap();
            if let (Some("200"), Some(body)) =
                (https.get("status").and_then(|x| x.as_number_text()), https.get("body").and_then(|b| b.as_str()))
            {
                rejected_doc |= s.put_namespace(host, body.as_bytes(), VALIDATED_AT).is_err();
            }
        }
        let want = datumesh_core::verify(&bytes, &keys(&v), None, None);
        match s.put_receipt(&bytes) {
            Ok(h) => {
                assert!(want.ok, "{id}: stored although verify says {:?}", want.code);
                stored += 1;
                let parsed = Receipt::from_json(&bytes).unwrap();
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
            Err(e) => {
                assert!(!want.ok, "{id}: refused ({e}) although it verifies");
                refused += 1;
                // verify's code; a key document the store refused (E_KEY_DOC) leaves the host
                // without keys in the store.
                let code = if want.code == Some(ErrorCode::KeyDoc) && rejected_doc {
                    Some(ErrorCode::KeyUnavailable)
                } else {
                    want.code
                };
                assert_eq!(e.code(), code, "{id}: {e}");
                assert_eq!(s.receipt(want.hash.as_deref().unwrap_or("")), None, "{id}");
            }
        }
    }
    assert!(stored >= 27, "{stored}");
    assert!(refused >= 25, "{refused}");
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
        let s = db.bare_store();
        put_vector_keys(&s, &v);
        s.put_descriptor(&desc.to_jcs()).unwrap();
        let bytes = input(&v);
        let h = s.put_receipt(&bytes).unwrap();
        let r = Receipt::from_json(&bytes).unwrap();
        s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
        let blob = v.get("blob_hex").and_then(|b| b.as_str()).map(hex);
        if let Some(content) = &blob {
            let sha = s.put_blob(content).unwrap();
            assert_eq!(s.blob(&sha).unwrap().as_deref(), Some(content.as_slice()));
        }
        let out = datumesh_core::verify(&bytes, &keys(&v), Some(&s as &dyn Resolver), blob.as_deref());
        assert!(out.ok, "{id}: {out:?}");
        assert_eq!(out.level, Some(datumesh_core::Level::Recomputed), "{id}: {out:?}");
        seen += 1;
    }
    assert!(seen >= 2, "{seen}");
}
