//! Manifests (0.3.1, SPEC 15.13): `put_manifest` parses strictly and checks the canonical
//! form and limits through the core (`Manifest::from_json`), stores the canonical form under
//! the manifest's entry, is idempotent and insert-only; `Resolver::manifest` serves it by
//! entry; each member receipt's manifests are found from the member (`member_manifests`).
//! A manifest does not need its members stored (members are fetched by hash, like inputs).

mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use common::*;
use datumesh_core::{ErrorCode, Manifest, Resolver};
use datumesh_store::{PoolOptions, Store};

#[test]
fn manifest_vectors_are_stored_under_their_entry() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let mut seen = 0;
    for v in vectors("slice").into_iter().filter(|v| str_of(v, "input").contains("\"members\"")) {
        let id = str_of(&v, "id");
        let bytes = input(&v);
        let entry = s.put_manifest(&bytes).unwrap();
        assert_eq!(entry, str_of(&v, "entry"), "{id}");
        // The canonical form, byte for byte; the same for the input put again, and for its
        // canonical form.
        let back = s.manifest(&entry).unwrap().into_owned();
        assert_eq!(back, jcs(&bytes), "{id}");
        assert_eq!(back, str_of(&v, "canonical").as_bytes(), "{id}");
        assert_eq!(s.put_manifest(&bytes).unwrap(), entry, "{id}");
        assert_eq!(s.put_manifest(&back).unwrap(), entry, "{id}");
        assert_eq!(&*s.manifest(&entry).unwrap(), back.as_slice(), "{id}");
        // Members need not be stored; each names this manifest.
        let m = Manifest::from_json(&bytes).unwrap();
        for h in m.members() {
            assert!(s.member_manifests(h).unwrap().contains(&entry), "{id}");
            assert_eq!(s.receipt(h), None);
        }
        seen += 1;
    }
    assert!(seen >= 3, "{seen}");
    assert_eq!(count(&db, "manifests"), seen);
}

#[test]
fn malformed_manifests_are_invalid_with_the_cores_codes() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let mut cases: Vec<(String, Vec<u8>)> = vectors("invalid")
        .into_iter()
        .chain(vectors("slice"))
        .filter(|v| str_of(v, "kind") == "slice")
        .map(|v| (str_of(&v, "id").to_string(), input(&v)))
        .collect();
    let good = str_of(&vector("slice/manifest"), "input").to_string();
    for (name, text) in [
        ("not JSON", "{".to_string()),
        ("duplicate member", good.replacen('{', r#"{"members":[],"#, 1)),
        ("extra member", good.replacen('{', r#"{"x":1,"#, 1)),
        ("null", "null".to_string()),
        ("an array", "[]".to_string()),
        ("a BOM", format!("\u{feff}{good}")),
        ("members not an array", good.replacen(r#""members": ["#, r#""members": {"a": ["#, 1).replacen(']', "]}", 1)),
    ] {
        cases.push((name.to_string(), text.into_bytes()));
    }
    let mut refused = 0;
    for (id, bytes) in &cases {
        match Manifest::from_json(bytes) {
            Ok(m) => assert_eq!(s.put_manifest(bytes).unwrap(), m.entry(), "{id}"),
            Err(core) => {
                let e = s.put_manifest(bytes).unwrap_err();
                assert_eq!(e.code(), Some(core.code()), "{id}: {e}");
                refused += 1;
            }
        }
    }
    // The manifest vectors that are invalid, and every slice object (not a manifest).
    assert!(refused >= 15, "{refused}");
    for v in vectors("invalid").into_iter().filter(|v| str_of(v, "id").starts_with("invalid/slice-manifest")) {
        let e = s.put_manifest(&input(&v)).unwrap_err();
        assert_eq!(e.code().map(ErrorCode::as_str), Some(str_of(&v, "error")), "{}", str_of(&v, "id"));
    }
}

#[test]
fn more_members_than_the_limit_are_refused() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let sl = r#"{"args":{"k":"*"},"fn":"f","fn_hash":"0000000000000000000000000000000000000000000000000000000000000000","ns":"example.org"}"#;
    let members: Vec<String> = (0..1_048_577u64).map(|i| format!("\"{i:064x}\"")).collect();
    let text = format!(r#"{{"members":[{}],"slice":{sl}}}"#, members.join(","));
    assert_eq!(s.put_manifest(text.as_bytes()).unwrap_err().code(), Some(ErrorCode::CanonLimit));
    assert_eq!(count(&db, "manifests"), 0);
}

#[test]
fn manifest_lookups_of_unknown_or_malformed_entries_are_none() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    let entry = s.put_manifest(&input(&vector("slice/manifest"))).unwrap();
    assert!(s.manifest(&entry).is_some());
    let h = "a".repeat(64);
    for bad in [
        format!("slice:{}:{}", "0".repeat(64), "0".repeat(64)),
        String::new(),
        "slice:".to_string(),
        entry.to_uppercase(),
        format!("{entry}0"),
        format!(" {entry}"),
        entry[..134].to_string(),
        entry.replacen("slice:", "SLICE:", 1),
        h.clone(),
        "' OR '1'='1".to_string(),
        "a".repeat(100_000),
        format!("{entry}\0"),
    ] {
        assert_eq!(s.manifest(&bad), None, "{bad:?}");
    }
    assert!(s.member_manifests(&h).unwrap().is_empty());
}

#[test]
fn manifests_are_insert_only_in_the_database() {
    let db = TestDb::migrated();
    let s = db.bare_store();
    s.put_manifest(&input(&vector("slice/manifest"))).unwrap();
    let mut c = db.client();
    for sql in [
        "UPDATE manifests SET bytes = bytes",
        "DELETE FROM manifests",
        "TRUNCATE manifests CASCADE",
        "UPDATE manifest_members SET member_hash = member_hash",
        "DELETE FROM manifest_members",
        "TRUNCATE manifest_members",
    ] {
        let e = c.batch_execute(sql).expect_err(sql);
        let msg = e.as_db_error().map(|d| d.message().to_string()).unwrap_or_default();
        assert!(msg.contains("append-only"), "{sql}: {msg}");
    }
    assert_eq!(count(&db, "manifests"), 1);
}

#[test]
fn concurrent_puts_of_one_manifest_store_it_once() {
    let db = TestDb::migrated();
    let s = Arc::new(db.bare_store_with(PoolOptions::new().max_size(8)));
    let sl = datumesh_core::Slice::from_json(
        br#"{"args":{"k":"*"},"fn":"f","fn_hash":"0000000000000000000000000000000000000000000000000000000000000000","ns":"example.org"}"#,
    )
    .unwrap();
    let members: Vec<String> = (0..10_000u64).map(|i| format!("{:064x}", i * 7 + 1)).collect();
    let m = Manifest::new(sl, members).unwrap();
    let bytes = Arc::new(m.canonical());
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let (s, bytes, barrier, url) = (s.clone(), bytes.clone(), barrier.clone(), db.url());
            thread::spawn(move || {
                // Half of the writers through their own store.
                let own = (i % 2 == 1).then(|| datumesh_store::PgStore::connect(&url).unwrap());
                barrier.wait();
                match &own {
                    Some(o) => o.put_manifest(&bytes),
                    None => s.put_manifest(&bytes),
                }
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap(), m.entry());
    }
    assert_eq!(count(&db, "manifests"), 1);
    assert_eq!(count(&db, "manifest_members"), 10_000);
    assert_eq!(&*s.manifest(&m.entry()).unwrap(), bytes.as_slice());
}
