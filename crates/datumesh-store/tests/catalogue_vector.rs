//! Vectors in the catalogue (0.5.0, migration 0009, CONTRACT-build2-part2-read R1):
//! the `embeddings` table, `put_embeddings`, `pending_embeddings` and `vector_find` on
//! PostgreSQL with pgvector. These run only when `DATUMESH_PGVECTOR_URL` names a server of
//! the `pgvector/pgvector:pg16` image (a role that may create databases); without it every
//! test returns at once. The migration on plain `postgres:16` is tested in `catalogue.rs`
//! (it must succeed there, with no embeddings table).
//!
//! Semantics specified here: embeddings are 384 floats; a model name keys them
//! (`(ref_kind, ref_id, model)` is unique, `put_embeddings` upserts); `vector_find` orders by
//! cosine similarity (score = 1 - cosine distance, so 1.0 for the same direction) and
//! answers entries and dictionary values as [`Hit`]s; `pending_embeddings(model, n)` lists
//! the entries and values that have no embedding for `model` (their text: for an entry
//! `"<label>. <description>"`, for a value its label, or the value when the label is empty);
//! changing an entry's text (an upsert that differs) or a value's label makes it pending
//! again, the old embedding being dropped.

mod common;

use std::collections::BTreeMap;

use common::*;
use datumesh_store::{
    CatalogEntry, CatalogKind, CatalogSource, EmbedRef, Embedding, Error, Hit, KeyValue, PendingEmbedding, Store,
};

const MODEL: &str = "intfloat/multilingual-e5-small";
const OWNER: &str = "scripture.example.org/verse";

fn url() -> Option<String> {
    std::env::var("DATUMESH_PGVECTOR_URL").ok().filter(|u| !u.is_empty())
}

/// A unit vector along axis `i` (384 dimensions), plus `y` along axis `j`.
fn axis(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 384];
    v[i] = 1.0;
    v
}

fn mix(i: usize, a: f32, j: usize, b: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; 384];
    v[i] = a;
    v[j] = b;
    v
}

fn entry(name: &str, label: &str, description: &str) -> CatalogEntry {
    CatalogEntry {
        kind: CatalogKind::Function,
        name: name.to_string(),
        hash: None,
        label: label.to_string(),
        description: description.to_string(),
        args: BTreeMap::new(),
        examples: "[]".to_string(),
        source: CatalogSource::Operator,
        signed_by: None,
    }
}

fn kv(value: &str, label: &str) -> KeyValue {
    KeyValue { value: value.to_string(), label: label.to_string(), aliases: vec![] }
}

fn migrated(url: &str) -> PgDb {
    let db = PgDb::empty_on(url);
    datumesh_store::migrate::apply(&db.bare_store()).unwrap();
    db
}

fn hit_name(h: &Hit) -> String {
    match h {
        Hit::Entry { entry, .. } => entry.name.clone(),
        Hit::Value { owner, key, value, .. } => format!("{owner}|{key}|{value}"),
        _ => unreachable!(),
    }
}

fn hit_score(h: &Hit) -> f64 {
    match h {
        Hit::Entry { score, .. } | Hit::Value { score, .. } => *score,
        _ => unreachable!(),
    }
}

fn scalar(db: &PgDb, sql: &str) -> String {
    db.client().query_one(sql, &[]).unwrap().get::<_, String>(0)
}

#[test]
fn the_migration_makes_embeddings_and_an_hnsw_cosine_index_where_vector_is_available() {
    let Some(url) = url() else { return };
    let db = migrated(&url);
    assert!(db.bare_store().vector_available());
    assert_eq!(scalar(&db, "SELECT count(*)::text FROM pg_extension WHERE extname = 'vector'"), "1");
    assert_eq!(scalar(&db, "SELECT count(*)::text FROM information_schema.tables WHERE table_name = 'embeddings' AND table_schema = current_schema()"), "1");
    let cols = db.client().query("SELECT column_name::text FROM information_schema.columns WHERE table_name = 'embeddings' AND table_schema = current_schema() ORDER BY column_name", &[]).unwrap();
    let cols: Vec<String> = cols.iter().map(|r| r.get(0)).collect();
    assert_eq!(cols, ["model", "ref_id", "ref_kind", "vec"]);
    // vector(384), and an HNSW index with cosine operators.
    assert_eq!(
        scalar(&db, "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a WHERE a.attrelid = 'embeddings'::regclass AND a.attname = 'vec'"),
        "vector(384)"
    );
    let idx = scalar(&db, "SELECT string_agg(indexdef, ' ') FROM pg_indexes WHERE tablename = 'embeddings'");
    assert!(idx.contains("hnsw") && idx.contains("vector_cosine_ops"), "{idx}");
}

#[test]
fn the_migration_is_repeatable_and_the_store_reports_vectors() {
    let Some(url) = url() else { return };
    let db = migrated(&url);
    assert!(datumesh_store::migrate::apply(&db.bare_store()).unwrap().is_empty());
    // The live schema equals what the migrations make in a scratch database of the same server.
    let scratch = migrated(&url);
    let problems = datumesh_store::migrate::check(&db.bare_store(), &scratch.bare_store()).unwrap();
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn vector_find_orders_by_cosine_similarity_over_entries_and_values() {
    let Some(url) = url() else { return };
    let db = migrated(&url);
    let s = db.bare_store();
    let e = s
        .upsert_catalog(&[entry("a.example.org/near", "Near", "Near text."), entry("a.example.org/far", "Far", "Far text.")])
        .unwrap();
    let v = s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis")]).unwrap();
    s.put_embeddings(
        MODEL,
        &[
            Embedding { ref_kind: EmbedRef::Entry, ref_id: e[0], vec: axis(0) },
            Embedding { ref_kind: EmbedRef::Entry, ref_id: e[1], vec: axis(2) },
            Embedding { ref_kind: EmbedRef::Value, ref_id: v[0], vec: mix(0, 1.0, 1, 1.0) },
        ],
    )
    .unwrap();
    let hits = s.vector_find(MODEL, &axis(0), 10).unwrap();
    assert_eq!(
        hits.iter().map(hit_name).collect::<Vec<_>>(),
        ["a.example.org/near".to_string(), format!("{OWNER}|book|Gen"), "a.example.org/far".to_string()]
    );
    let sc: Vec<f64> = hits.iter().map(hit_score).collect();
    assert!((sc[0] - 1.0).abs() < 1e-4, "same direction: {sc:?}");
    assert!((sc[1] - 0.7071).abs() < 1e-3, "45 degrees: {sc:?}");
    assert!(sc[2].abs() < 1e-4, "orthogonal: {sc:?}");
    // Entries carry their fields, values theirs.
    match &hits[1] {
        Hit::Value { id, label, .. } => assert_eq!((*id, label.as_str()), (v[0], "Genesis")),
        other => panic!("{other:?}"),
    }
    assert_eq!(s.vector_find(MODEL, &axis(0), 2).unwrap().len(), 2);
    assert!(s.vector_find(MODEL, &axis(0), 0).unwrap().is_empty());
    // Another model's space is separate.
    assert!(s.vector_find("other-model", &axis(0), 10).unwrap().is_empty());
}

#[test]
fn a_vector_of_the_wrong_size_is_refused() {
    let Some(url) = url() else { return };
    let db = migrated(&url);
    let s = db.bare_store();
    let e = s.upsert_catalog(&[entry("a.example.org/f", "F", "Text.")]).unwrap();
    let short = Embedding { ref_kind: EmbedRef::Entry, ref_id: e[0], vec: vec![1.0; 3] };
    assert!(matches!(s.put_embeddings(MODEL, &[short]), Err(Error::Invalid(_))));
    assert!(matches!(s.vector_find(MODEL, &[1.0; 3], 5), Err(Error::Invalid(_))));
    assert_eq!(db.client().query_one("SELECT count(*) FROM embeddings", &[]).unwrap().get::<_, i64>(0), 0);
}

#[test]
fn put_embeddings_upserts_and_refuses_a_reference_that_is_not_there() {
    let Some(url) = url() else { return };
    let db = migrated(&url);
    let s = db.bare_store();
    let e = s.upsert_catalog(&[entry("a.example.org/f", "F", "Text.")]).unwrap();
    let put = |v: Vec<f32>| s.put_embeddings(MODEL, &[Embedding { ref_kind: EmbedRef::Entry, ref_id: e[0], vec: v }]).unwrap();
    put(axis(0));
    put(axis(1));
    let n: i64 = db.client().query_one("SELECT count(*) FROM embeddings", &[]).unwrap().get(0);
    assert_eq!(n, 1, "one embedding per (kind, id, model)");
    let hits = s.vector_find(MODEL, &axis(1), 5).unwrap();
    assert!((hit_score(&hits[0]) - 1.0).abs() < 1e-4, "the second vector replaced the first");
    // A reference to nothing is refused, and the batch writes nothing.
    let ok = Embedding { ref_kind: EmbedRef::Entry, ref_id: e[0], vec: axis(3) };
    let bad = Embedding { ref_kind: EmbedRef::Value, ref_id: 987_654, vec: axis(4) };
    assert!(s.put_embeddings("m2", &[ok, bad]).is_err());
    let n: i64 = db.client().query_one("SELECT count(*) FROM embeddings WHERE model = 'm2'", &[]).unwrap().get(0);
    assert_eq!(n, 0);
}

#[test]
fn pending_embeddings_lists_what_has_none_for_the_model_and_is_idempotent() {
    let Some(url) = url() else { return };
    let db = migrated(&url);
    let s = db.bare_store();
    let e = s.upsert_catalog(&[entry("a.example.org/f", "Label F", "Text of F."), entry("a.example.org/g", "G", "Text of G.")]).unwrap();
    let v = s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis"), kv("Exo", "")]).unwrap();
    let pend = |model: &str| -> Vec<PendingEmbedding> { s.pending_embeddings(model, 100).unwrap() };
    let text = |p: &[PendingEmbedding], kind: EmbedRef, id: i64| -> String {
        p.iter().find(|x| x.ref_kind == kind && x.ref_id == id).unwrap_or_else(|| panic!("{kind:?} {id} pending")).text.clone()
    };
    let p = pend(MODEL);
    assert_eq!(p.len(), 4);
    assert_eq!(text(&p, EmbedRef::Entry, e[0]), "Label F. Text of F.");
    assert_eq!(text(&p, EmbedRef::Value, v[0]), "Genesis");
    assert_eq!(text(&p, EmbedRef::Value, v[1]), "Exo", "the value when there is no label");
    assert_eq!(s.pending_embeddings(MODEL, 3).unwrap().len(), 3);

    s.put_embeddings(MODEL, &[Embedding { ref_kind: EmbedRef::Entry, ref_id: e[0], vec: axis(0) }, Embedding { ref_kind: EmbedRef::Value, ref_id: v[0], vec: axis(1) }]).unwrap();
    assert_eq!(pend(MODEL).len(), 2);
    assert_eq!(pend("another-model").len(), 4, "per model");
    // The same text upserted again keeps its embedding.
    s.upsert_catalog(&[entry("a.example.org/f", "Label F", "Text of F.")]).unwrap();
    s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis")]).unwrap();
    assert_eq!(pend(MODEL).len(), 2);
    // A changed text does not.
    s.upsert_catalog(&[entry("a.example.org/f", "Label F", "Text of F, rewritten.")]).unwrap();
    s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis, the first book")]).unwrap();
    let p = pend(MODEL);
    assert_eq!(p.len(), 4);
    assert_eq!(text(&p, EmbedRef::Entry, e[0]), "Label F. Text of F, rewritten.");
    assert!(s.vector_find(MODEL, &axis(0), 5).unwrap().is_empty(), "the stale embeddings are gone");
}
