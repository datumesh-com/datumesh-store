//! The catalogue (0.5.0, migration 0009; CONTRACT-build2-part2-read R1): `catalog_entries`,
//! `key_values` and `find_misses` on both backends (embeddings only on PostgreSQL with the
//! `vector` extension, see `catalogue_vector.rs`), and the store API over them:
//! `upsert_catalog`, `upsert_key_values`, `refresh_value_counts`, `text_find`, `log_miss`,
//! `catalog_summary`, and the vector calls' refusal where vectors are not available.
//!
//! API specified by these tests (all on `Store`, additions only):
//!
//! ```text
//! fn upsert_catalog(&self, entries: &[CatalogEntry]) -> Result<Vec<i64>>;      // ids, in order
//! fn upsert_key_values(&self, owner: &str, key: &str, values: &[KeyValue]) -> Result<Vec<i64>>;
//! fn refresh_value_counts(&self) -> Result<u64>;                               // rows updated
//! fn text_find(&self, query: &str, limit: usize) -> Result<Vec<Hit>>;
//! fn vector_find(&self, model: &str, vec: &[f32], limit: usize) -> Result<Vec<Hit>>;
//! fn put_embeddings(&self, model: &str, items: &[Embedding]) -> Result<()>;
//! fn pending_embeddings(&self, model: &str, limit: usize) -> Result<Vec<PendingEmbedding>>;
//! fn vector_available(&self) -> bool;
//! fn log_miss(&self, query: &str) -> Result<()>;
//! fn catalog_summary(&self) -> Result<Summary>;
//! ```
//!
//! Every test here fails until the behaviour is implemented (it does not compile before).

#[macro_use]
mod common;

use std::collections::BTreeMap;

use common::*;
use datumesh_core::Type;
use datumesh_store::{
    migrate, CatalogEntry, CatalogKind, CatalogSource, EmbedRef, Embedding, Error, Hit, KeyValue, FIND_MISSES_CAP,
};

const OWNER: &str = "scripture.example.org/verse";
const CLASS_NAME: &str = "scripture.example.org/passage";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
#[allow(dead_code)] // (no test in this file uses the second hash; kept for the entry tests' symmetry)
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn entry(kind: CatalogKind, name: &str, label: &str, description: &str) -> CatalogEntry {
    CatalogEntry {
        kind,
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

fn function(name: &str, label: &str, description: &str) -> CatalogEntry {
    entry(CatalogKind::Function, name, label, description)
}

fn kv(value: &str, label: &str, aliases: &[&str]) -> KeyValue {
    KeyValue {
        value: value.to_string(),
        label: label.to_string(),
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
    }
}

fn table_exists(db: &TestDb, table: &str) -> bool {
    // (The format! results are bound first: `db.pick` borrows them.)
    let pg = format!("SELECT count(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = '{table}'");
    let lite = format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{table}'");
    let sql = db.pick(&pg, &lite);
    db.int(sql) == 1
}

fn columns(db: &TestDb, table: &str) -> Vec<String> {
    let pg = format!("SELECT column_name::text FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = '{table}' ORDER BY column_name");
    let lite = format!("SELECT name FROM pragma_table_info('{table}') ORDER BY name");
    let sql = db.pick(&pg, &lite);
    db.texts(sql).into_iter().map(|r| r[0].clone()).collect()
}

fn names(hits: &[Hit]) -> Vec<String> {
    hits.iter()
        .map(|h| match h {
            Hit::Entry { entry, .. } => entry.name.clone(),
            Hit::Value { owner, key, value, .. } => format!("{owner}|{key}|{value}"),
            _ => unreachable!("only entries and values"),
        })
        .collect()
}

fn score(h: &Hit) -> f64 {
    match h {
        Hit::Entry { score, .. } | Hit::Value { score, .. } => *score,
        _ => unreachable!(),
    }
}

// ---- The migration ------------------------------------------------------------------------

fn migration_makes_the_catalogue_tables_and_no_embeddings_without_vectors(b: Backend) {
    let db = TestDb::migrated(b);
    for t in ["catalog_entries", "key_values", "find_misses"] {
        assert!(table_exists(&db, t), "{t} exists");
    }
    // The tests' PostgreSQL (DATABASE_URL) is plain postgres:16: no vector extension, so no
    // embeddings table and vector_available is false. SQLite never has vectors.
    assert!(!table_exists(&db, "embeddings"));
    assert!(!db.store().vector_available());
}

fn migrations_are_registered(b: Backend) {
    let last = match b {
        Backend::Pg => migrate::MIGRATIONS.last().unwrap(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.last().unwrap(),
    };
    match b {
        Backend::Pg => assert_eq!((last.version, last.name), (9, "0009_catalogue.sql")),
        Backend::Sqlite => assert_eq!((last.version, last.name), (5, "0005_catalogue.sql")),
    }
}

fn the_migration_is_additive_and_repeatable(b: Backend) {
    let db = TestDb::migrated(b);
    let again = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(datumesh_store::SqliteOptions::new())).unwrap(),
    };
    assert!(again.is_empty(), "nothing left to apply");
}

fn find_misses_has_the_query_and_the_time_and_nothing_else(b: Backend) {
    let db = TestDb::migrated(b);
    assert_eq!(columns(&db, "find_misses"), ["at", "query"]);
}

fn the_tables_refuse_what_the_contract_bounds(b: Backend) {
    let db = TestDb::migrated(b);
    let ins = |kind: &str, label: &str, desc: &str| {
        db.exec(&format!(
            "INSERT INTO catalog_entries (kind, name, label, description, args, examples, source) \
             VALUES ('{kind}', 'x.example.org/f', '{label}', '{desc}', '{{}}', '[]', 'operator')"
        ))
    };
    assert!(ins("function", "ok", "ok").is_ok());
    db.exec("DELETE FROM catalog_entries").unwrap(); // the catalogue is not append-only
    assert!(ins("bogus", "ok", "ok").is_err(), "kind is class, method or function");
    assert!(ins("function", "ok", "").is_err(), "a description has 1 to 1024 bytes");
    assert!(ins("function", "ok", &"d".repeat(1025)).is_err());
    assert!(ins("function", &"l".repeat(121), "ok").is_err(), "a label has at most 120 bytes");
    assert!(
        db.exec(
            "INSERT INTO catalog_entries (kind, name, label, description, args, examples, source) \
             VALUES ('function', 'x.example.org/g', 'l', 'd', '{}', '[]', 'nobody')"
        )
        .is_err(),
        "source is publisher or operator"
    );
}

// ---- upsert_catalog -----------------------------------------------------------------------

fn an_entry_round_trips(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let mut e = function(OWNER, "Verse of a book", "A verse of a book, as plain text.");
    e.hash = Some(HASH_A.to_string());
    e.args.insert("book".into(), "the book code, such as the codes of the dictionary".into());
    e.args.insert("chapter".into(), "the chapter number".into());
    e.examples = r#"[{"call":"scripture.example.org/verse(book: \"Xyz\", chapter: 1)","expected":{"type":"string","value":"synthetic"}}]"#.into();
    e.source = CatalogSource::Publisher;
    e.signed_by = Some("scripture.example.org#key-1".into());
    let ids = s.upsert_catalog(std::slice::from_ref(&e)).unwrap();
    assert_eq!(ids.len(), 1);
    let hits = s.text_find("verse of a book", 10).unwrap();
    assert_eq!(hits.len(), 1);
    match &hits[0] {
        Hit::Entry { id, entry, .. } => {
            assert_eq!(*id, ids[0]);
            assert_eq!(entry.kind, CatalogKind::Function);
            assert_eq!(entry.name, OWNER);
            assert_eq!(entry.hash.as_deref(), Some(HASH_A));
            assert_eq!(entry.label, e.label);
            assert_eq!(entry.description, e.description);
            assert_eq!(entry.args, e.args);
            assert_eq!(entry.source, CatalogSource::Publisher);
            assert_eq!(entry.signed_by, e.signed_by);
            // The examples are JSON: equal as a value, not necessarily as text.
            let a = datumesh_core::json::parse(entry.examples.as_bytes()).unwrap().to_jcs();
            let w = datumesh_core::json::parse(e.examples.as_bytes()).unwrap().to_jcs();
            assert_eq!(a, w);
        }
        other => panic!("an entry, got {other:?}"),
    }
}

fn upserting_the_same_kind_and_name_updates_in_place(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let first = s.upsert_catalog(&[function(OWNER, "Verse", "First description.")]).unwrap();
    let second = s.upsert_catalog(&[function(OWNER, "Verse v2", "Second description.")]).unwrap();
    assert_eq!(first, second, "the same row");
    assert_eq!(db.count("catalog_entries"), 1);
    let hits = s.text_find("second", 10).unwrap();
    assert_eq!(names(&hits), [OWNER]);
    assert!(s.text_find("first", 10).unwrap().is_empty(), "the old text is gone");
    // A batch with the same entry twice is one row.
    s.upsert_catalog(&[function(OWNER, "A", "dup one"), function(OWNER, "B", "dup two")]).unwrap();
    assert_eq!(db.count("catalog_entries"), 1);
}

fn kind_and_name_together_are_unique(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let ids = s
        .upsert_catalog(&[
            entry(CatalogKind::Class, CLASS_NAME, "Passage", "A class of passages."),
            entry(CatalogKind::Method, CLASS_NAME, "Passage method", "A method of passages."),
            entry(CatalogKind::Function, CLASS_NAME, "Passage function", "A function of passages."),
        ])
        .unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(db.count("catalog_entries"), 3);
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "three distinct ids");
}

fn entries_outside_the_bounds_are_refused_and_nothing_is_written(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let good = function(OWNER, "Verse", "A verse.");
    for bad in [
        function("x.example.org/a", "ok", ""),
        function("x.example.org/a", "ok", &"d".repeat(1025)),
        function("x.example.org/a", &"l".repeat(121), "ok"),
        function("", "ok", "ok"),
    ] {
        let err = s.upsert_catalog(&[good.clone(), bad]).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err:?}");
        assert_eq!(db.count("catalog_entries"), 0, "the batch is all or nothing");
    }
    // The bounds are inclusive: 120 bytes of label, 1 and 1024 bytes of description.
    s.upsert_catalog(&[
        function("x.example.org/a", &"l".repeat(120), "d"),
        function("x.example.org/b", "l", &"d".repeat(1024)),
    ])
    .unwrap();
    // Bytes, not characters: 61 two-byte characters are 122 bytes.
    let err = s.upsert_catalog(&[function("x.example.org/c", &"é".repeat(61), "ok")]).unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));
}

fn an_operator_entry_never_replaces_a_publisher_entry(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let mut p = function(OWNER, "Publisher label", "Publisher text about verses.");
    p.source = CatalogSource::Publisher;
    p.signed_by = Some("scripture.example.org#key-1".into());
    let id = s.upsert_catalog(&[p.clone()]).unwrap();
    // The operator's entry for the same name is not applied; the call is not an error and
    // answers the existing row's id.
    let o = function(OWNER, "Operator label", "Operator text about verses.");
    assert_eq!(s.upsert_catalog(std::slice::from_ref(&o)).unwrap(), id);
    let hits = s.text_find("publisher", 10).unwrap();
    assert_eq!(names(&hits), [OWNER]);
    assert!(s.text_find("operator", 10).unwrap().is_empty());
    // It may add an entry where there is no publisher entry, and a publisher entry replaces
    // an operator one.
    let mut o2 = function("scripture.example.org/other", "Other", "Operator only.");
    o2.source = CatalogSource::Operator;
    s.upsert_catalog(&[o2.clone()]).unwrap();
    assert_eq!(names(&s.text_find("operator only", 10).unwrap()), ["scripture.example.org/other"]);
    let mut p2 = o2.clone();
    p2.source = CatalogSource::Publisher;
    p2.description = "Now from the publisher.".into();
    p2.signed_by = Some("scripture.example.org#key-1".into());
    s.upsert_catalog(&[p2]).unwrap();
    assert!(s.text_find("operator only", 10).unwrap().is_empty());
    match &s.text_find("now from the publisher", 10).unwrap()[0] {
        Hit::Entry { entry, .. } => assert_eq!(entry.source, CatalogSource::Publisher),
        other => panic!("{other:?}"),
    }
    assert_eq!(db.count("catalog_entries"), 2);
}

// ---- text_find ----------------------------------------------------------------------------

fn world(s: &TStore) {
    s.upsert_catalog(&[
        function(OWNER, "Verse of a book", "The text of one verse, chosen by book and chapter."),
        function("rates.example.org/fx_rate", "Exchange rate", "The daily reference rate of a currency pair."),
        entry(CatalogKind::Class, CLASS_NAME, "Passage", "A passage of a book: the verse functions share its keys."),
    ])
    .unwrap();
    s.upsert_key_values(
        OWNER,
        "book",
        &[kv("Gen", "Genesis", &["1Mo"]), kv("2Chr", "2 Chronicles", &["Second Chronicles"])],
    )
    .unwrap();
}

fn text_find_needs_every_word_in_name_label_description_or_args(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    world(&s);
    let mut with_args = function("calc.example.net/convert", "Convert", "Converts an amount.");
    with_args.args.insert("amount".into(), "the quantity to convert in the source currency".into());
    s.upsert_catalog(&[with_args]).unwrap();

    // Words may be found in different fields of one entry.
    assert_eq!(names(&s.text_find("fx_rate daily", 10).unwrap()), ["rates.example.org/fx_rate"]); // name + description
    assert_eq!(names(&s.text_find("exchange currency", 10).unwrap()), ["rates.example.org/fx_rate"]); // label + description
    assert_eq!(names(&s.text_find("converts quantity", 10).unwrap()), ["calc.example.net/convert"]); // description + args
                                                                                                     // Every word must be found.
    assert!(s.text_find("daily zebra", 10).unwrap().is_empty());
    // Case-insensitive.
    assert_eq!(names(&s.text_find("EXCHANGE RATE", 10).unwrap()), ["rates.example.org/fx_rate"]);
    // A blank query finds nothing.
    assert!(s.text_find("", 10).unwrap().is_empty());
    assert!(s.text_find("   ", 10).unwrap().is_empty());
}

fn text_find_treats_wildcards_literally(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    world(&s);
    for q in ["%", "fx_rat_", "ver_e", "100%", "\\"] {
        assert!(s.text_find(q, 10).unwrap().is_empty(), "{q:?}");
    }
    s.upsert_catalog(&[function("x.example.org/p", "Percent", "Grows by 5% a year.")]).unwrap();
    assert_eq!(names(&s.text_find("5%", 10).unwrap()), ["x.example.org/p"]);
}

fn text_find_finds_dictionary_values_by_value_label_and_alias(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    world(&s);
    for q in ["Gen", "gen", "Genesis", "genesis", "1Mo", "1mo"] {
        let hits = s.text_find(q, 10).unwrap();
        let v: Vec<_> = hits.iter().filter(|h| matches!(h, Hit::Value { .. })).collect();
        assert_eq!(v.len(), 1, "{q}: {hits:?}");
        match v[0] {
            Hit::Value { owner, key, value, label, .. } => {
                assert_eq!(
                    (owner.as_str(), key.as_str(), value.as_str(), label.as_str()),
                    (OWNER, "book", "Gen", "Genesis")
                );
            }
            _ => unreachable!(),
        }
    }
    // Several words of a label.
    let hits = s.text_find("second chronicles", 10).unwrap();
    assert!(names(&hits).contains(&format!("{OWNER}|book|2Chr")), "{hits:?}");
    let hits = s.text_find("2 chronicles", 10).unwrap();
    assert!(names(&hits).contains(&format!("{OWNER}|book|2Chr")), "{hits:?}");
}

fn text_find_ranks_a_whole_match_first_and_scores_fall_in_zero_to_one(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    world(&s);
    // "passage" is the whole label of the class and a word of the description of nothing
    // else; add an entry that only mentions it.
    s.upsert_catalog(&[function("x.example.org/mention", "Mention", "Reads a passage from somewhere.")]).unwrap();
    let hits = s.text_find("passage", 10).unwrap();
    let n = names(&hits);
    assert_eq!(n[0], CLASS_NAME, "{n:?}");
    assert!(n.contains(&"x.example.org/mention".to_string()));
    assert!(score(&hits[0]) > score(&hits[1]));
    for h in &hits {
        assert!(score(h) > 0.0 && score(h) <= 1.0);
    }
    // Scores never rise down the list.
    assert!(hits.windows(2).all(|w| score(&w[0]) >= score(&w[1])));
}

fn text_find_honours_the_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let many: Vec<CatalogEntry> = (0..12)
        .map(|i| function(&format!("x.example.org/f{i:02}"), "Common label", &format!("Common text {i}.")))
        .collect();
    s.upsert_catalog(&many).unwrap();
    assert_eq!(s.text_find("common", 5).unwrap().len(), 5);
    assert_eq!(s.text_find("common", 50).unwrap().len(), 12);
    assert!(s.text_find("common", 0).unwrap().is_empty());
    // Equal scores come in a fixed order (by name), so the same call gives the same answer.
    assert_eq!(names(&s.text_find("common", 5).unwrap()), names(&s.text_find("common", 5).unwrap()));
}

// ---- upsert_key_values --------------------------------------------------------------------

fn key_values_upsert_replaces_label_and_aliases_and_keeps_counts(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let ids = s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis", &["1Mo"]), kv("Exo", "Exodus", &[])]).unwrap();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    db.exec("UPDATE key_values SET n_cells = 7 WHERE value = 'Gen'").unwrap();
    let again = s.upsert_key_values(OWNER, "book", &[kv("Gen", "Genesis (renamed)", &["Bereshit"])]).unwrap();
    assert_eq!(again, vec![ids[0]], "the same row");
    assert_eq!(db.count("key_values"), 2, "Exo is kept: an upsert adds and replaces, it does not prune");
    assert_eq!(
        db.int("SELECT n_cells FROM key_values WHERE value = 'Gen'"),
        7,
        "the count is the store's, not the dictionary's"
    );
    assert!(s
        .text_find("bereshit", 10)
        .unwrap()
        .iter()
        .any(|h| matches!(h, Hit::Value { value, .. } if value == "Gen")));
    assert!(s.text_find("1Mo", 10).unwrap().is_empty(), "the old alias is gone");
    assert!(s
        .text_find("renamed", 10)
        .unwrap()
        .iter()
        .any(|h| matches!(h, Hit::Value { value, .. } if value == "Gen")));
}

fn key_values_are_per_owner_and_key(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.upsert_key_values("a.example.org/f", "k", &[kv("X", "First X", &[])]).unwrap();
    s.upsert_key_values("a.example.org/f", "j", &[kv("X", "Second X", &[])]).unwrap();
    s.upsert_key_values("a.example.org/g", "k", &[kv("X", "Third X", &[])]).unwrap();
    assert_eq!(db.count("key_values"), 3);
    // The default count is zero.
    assert_eq!(db.int("SELECT max(n_cells) FROM key_values"), 0);
    assert!(s.upsert_key_values("a.example.org/f", "k", &[]).unwrap().is_empty());
}

fn refresh_value_counts_counts_the_cells_of_the_owners_function(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h =
        s.put_descriptor(&stored_descriptor(&[("book", Type::String), ("chapter", Type::Number)], "verse")).unwrap();
    let mut e = function(OWNER, "Verse", "A verse.");
    e.hash = Some(h.clone());
    s.upsert_catalog(&[e]).unwrap();
    let cell = |book: &str, ch: &str| {
        slice_cell("verse", &h, &[("book", sc(Type::String, book)), ("chapter", sc(Type::Number, ch))])
    };
    for (book, ch) in [("John", "1"), ("John", "2"), ("John", "3"), ("Gen", "1")] {
        put_member(&s, &cell(book, ch), "1", T1, T1);
    }
    // A second version of a cell is not another cell.
    put_member(&s, &cell("Gen", "1"), "2", "2024-03-16T16:00:00.000000Z", "2024-03-16T16:00:00.000000Z");
    s.upsert_key_values(OWNER, "book", &[kv("John", "John", &[]), kv("Gen", "Genesis", &[]), kv("Exo", "Exodus", &[])])
        .unwrap();
    // A dictionary of an owner with no entry has nothing to count.
    s.upsert_key_values("nobody.example.org/f", "book", &[kv("John", "John", &[])]).unwrap();

    let n = s.refresh_value_counts().unwrap();
    assert!(n >= 3, "the rows of the owner were refreshed, got {n}");
    let counts = |owner: &str| -> Vec<(String, i64)> {
        db.texts(&format!("SELECT value, CAST(n_cells AS TEXT) FROM key_values WHERE owner = '{owner}' ORDER BY value"))
            .into_iter()
            .map(|r| (r[0].clone(), r[1].parse().unwrap()))
            .collect()
    };
    assert_eq!(counts(OWNER), [("Exo".to_string(), 0), ("Gen".into(), 1), ("John".into(), 3)]);
    assert_eq!(counts("nobody.example.org/f"), [("John".to_string(), 0)]);

    // More cells, refreshed again; idempotent when nothing changed.
    put_member(&s, &cell("Exo", "1"), "1", T1, T1);
    s.refresh_value_counts().unwrap();
    s.refresh_value_counts().unwrap();
    assert_eq!(counts(OWNER), [("Exo".to_string(), 1), ("Gen".into(), 1), ("John".into(), 3)]);
}

// ---- log_miss -----------------------------------------------------------------------------

fn a_miss_is_the_query_text_and_a_time(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.log_miss("nothing like this").unwrap();
    s.log_miss("nothing like this").unwrap();
    s.log_miss("zebra").unwrap();
    // Each call is a row (the demand record counts).
    assert_eq!(db.count("find_misses"), 3);
    let rows = db.texts("SELECT query FROM find_misses WHERE query = 'nothing like this'");
    assert_eq!(rows.len(), 2);
    let recent = db.int(db.pick(
        "SELECT count(*) FROM find_misses WHERE at > now() - interval '1 minute' AND at <= now() + interval '1 minute'",
        "SELECT count(*) FROM find_misses WHERE at >= strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 minute')",
    ));
    assert_eq!(recent, 3, "at is the time of the call");
}

fn a_long_miss_is_cut_to_512_bytes_on_a_character_boundary(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.log_miss(&"q".repeat(600)).unwrap();
    s.log_miss(&"é".repeat(300)).unwrap(); // 600 bytes: cut to 256 characters
    s.log_miss(&"q".repeat(512)).unwrap();
    let mut lens: Vec<usize> = db.texts("SELECT query FROM find_misses").into_iter().map(|r| r[0].len()).collect();
    lens.sort();
    assert_eq!(lens, [512, 512, 512]);
    let e = db.texts("SELECT query FROM find_misses WHERE query LIKE 'é%'");
    assert_eq!(e[0][0], "é".repeat(256));
}

fn a_blank_miss_is_not_recorded(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.log_miss("").unwrap();
    s.log_miss("  \t ").unwrap();
    assert_eq!(db.count("find_misses"), 0);
}

fn the_cap_is_100_000() {
    assert_eq!(FIND_MISSES_CAP, 100_000);
}

fn misses_beyond_the_cap_prune_the_oldest(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let extra = 10;
    let n = FIND_MISSES_CAP + extra;
    // Fill beyond the cap with raw SQL (old times, one second apart), as an earlier store
    // version or a burst might have left it.
    let (pg_sql, lite_sql) = (
        format!("INSERT INTO find_misses (query, at) SELECT 'old ' || i, timestamptz '2020-01-01 00:00:00+00' + i * interval '1 second' FROM generate_series(1, {n}) AS i"),
        format!("WITH RECURSIVE g(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM g WHERE i < {n}) INSERT INTO find_misses (query, at) SELECT 'old ' || i, strftime('%Y-%m-%dT%H:%M:%fZ', '2020-01-01 00:00:00', '+' || i || ' seconds') FROM g"),
    );
    db.exec(db.pick(&pg_sql, &lite_sql)).unwrap();
    assert_eq!(db.count("find_misses"), n as i64);
    s.log_miss("the newest").unwrap();
    // One more in, the cap holds: the (extra + 1) oldest are gone.
    assert_eq!(db.count("find_misses"), FIND_MISSES_CAP as i64);
    assert_eq!(db.count("find_misses WHERE query = 'the newest'"), 1);
    assert_eq!(db.count("find_misses WHERE query = 'old 1'"), 0);
    assert_eq!(db.count(&format!("find_misses WHERE query = 'old {}'", extra + 1)), 0);
    assert_eq!(db.count(&format!("find_misses WHERE query = 'old {}'", extra + 2)), 1);
    assert_eq!(db.count(&format!("find_misses WHERE query = 'old {n}'")), 1);
    // And it keeps holding.
    s.log_miss("another").unwrap();
    assert_eq!(db.count("find_misses"), FIND_MISSES_CAP as i64);
    assert_eq!(db.count(&format!("find_misses WHERE query = 'old {}'", extra + 2)), 0);
}

// ---- catalog_summary ----------------------------------------------------------------------

fn the_summary_lists_entries_with_labels_and_cell_counts_and_small_dictionaries(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("book", Type::String)], "verse")).unwrap();
    let mut e = function(OWNER, "Verse of a book", "A verse.");
    e.hash = Some(h.clone());
    s.upsert_catalog(&[
        e,
        entry(CatalogKind::Class, CLASS_NAME, "Passage", "A class of passages."),
        entry(CatalogKind::Method, &format!("{CLASS_NAME}/verse"), "Passage verse", "A method."),
    ])
    .unwrap();
    for (i, book) in ["John", "John", "Gen"].iter().enumerate() {
        let cell = slice_cell("verse", &h, &[("book", sc(Type::String, &format!("{book}{i}")))]);
        put_member(&s, &cell, "1", T1, T1);
    }
    s.upsert_key_values(
        OWNER,
        "book",
        &[kv("John0", "John (a)", &[]), kv("John1", "John (b)", &[]), kv("Gen2", "Genesis", &[])],
    )
    .unwrap();
    s.refresh_value_counts().unwrap();

    let sum = s.catalog_summary().unwrap();
    let find = |kind: CatalogKind, name: &str| {
        sum.entries
            .iter()
            .find(|e| e.kind == kind && e.name == name)
            .unwrap_or_else(|| panic!("{name} in {:?}", sum.entries))
    };
    let f = find(CatalogKind::Function, OWNER);
    assert_eq!((f.label.as_str(), f.n_cells), ("Verse of a book", 3));
    let c = find(CatalogKind::Class, CLASS_NAME);
    assert_eq!((c.label.as_str(), c.n_cells), ("Passage", 0));
    assert_eq!(find(CatalogKind::Method, &format!("{CLASS_NAME}/verse")).label, "Passage verse");
    assert_eq!(sum.entries.len(), 3);

    assert_eq!(sum.dictionaries.len(), 1);
    let d = &sum.dictionaries[0];
    assert_eq!((d.owner.as_str(), d.key.as_str(), d.n_values), (OWNER, "book", 3));
    let vals: Vec<(&str, &str, i64)> =
        d.values.iter().map(|v| (v.value.as_str(), v.label.as_str(), v.n_cells)).collect();
    assert_eq!(vals, [("Gen2", "Genesis", 1), ("John0", "John (a)", 1), ("John1", "John (b)", 1)], "ordered by value");
}

fn a_dictionary_shows_at_most_50_values_and_says_how_many_there_are(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let many: Vec<KeyValue> = (0..60).map(|i| kv(&format!("v{i:02}"), &format!("Value {i}"), &[])).collect();
    s.upsert_key_values("x.example.org/f", "k", &many).unwrap();
    let few: Vec<KeyValue> = (0..50).map(|i| kv(&format!("w{i:02}"), &format!("W {i}"), &[])).collect();
    s.upsert_key_values("x.example.org/f", "j", &few).unwrap();
    let sum = s.catalog_summary().unwrap();
    let d = |key: &str| sum.dictionaries.iter().find(|d| d.key == key).unwrap();
    assert_eq!(d("k").values.len(), 50);
    assert_eq!(d("k").n_values, 60);
    assert_eq!(d("k").values[0].value, "v00");
    assert_eq!(d("k").values[49].value, "v49");
    assert_eq!(d("j").values.len(), 50);
    assert_eq!(d("j").n_values, 50);
}

fn an_empty_catalogue_has_an_empty_summary(b: Backend) {
    let db = TestDb::migrated(b);
    let sum = db.store().catalog_summary().unwrap();
    assert!(sum.entries.is_empty() && sum.dictionaries.is_empty());
}

// ---- Vectors where there are none ---------------------------------------------------------

fn vector_calls_are_refused_without_the_extension(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    assert!(!s.vector_available());
    let ids = s.upsert_catalog(&[function(OWNER, "Verse", "A verse.")]).unwrap();
    let v = vec![0.0f32; 384];
    assert!(matches!(s.vector_find("m", &v, 5), Err(Error::Unsupported(_))));
    let item = Embedding { ref_kind: EmbedRef::Entry, ref_id: ids[0], vec: v };
    assert!(matches!(s.put_embeddings("m", &[item]), Err(Error::Unsupported(_))));
    // Nothing to embed where nothing can be stored: not an error, an empty list.
    assert!(s.pending_embeddings("m", 10).unwrap().is_empty());
    // Text find is unaffected.
    assert_eq!(names(&s.text_find("verse", 5).unwrap()), [OWNER]);
}

backends!(
    migration_makes_the_catalogue_tables_and_no_embeddings_without_vectors,
    migrations_are_registered,
    the_migration_is_additive_and_repeatable,
    find_misses_has_the_query_and_the_time_and_nothing_else,
    the_tables_refuse_what_the_contract_bounds,
    an_entry_round_trips,
    upserting_the_same_kind_and_name_updates_in_place,
    kind_and_name_together_are_unique,
    entries_outside_the_bounds_are_refused_and_nothing_is_written,
    an_operator_entry_never_replaces_a_publisher_entry,
    text_find_needs_every_word_in_name_label_description_or_args,
    text_find_treats_wildcards_literally,
    text_find_finds_dictionary_values_by_value_label_and_alias,
    text_find_ranks_a_whole_match_first_and_scores_fall_in_zero_to_one,
    text_find_honours_the_limit,
    key_values_upsert_replaces_label_and_aliases_and_keeps_counts,
    key_values_are_per_owner_and_key,
    refresh_value_counts_counts_the_cells_of_the_owners_function,
    a_miss_is_the_query_text_and_a_time,
    a_long_miss_is_cut_to_512_bytes_on_a_character_boundary,
    a_blank_miss_is_not_recorded,
    misses_beyond_the_cap_prune_the_oldest,
    the_summary_lists_entries_with_labels_and_cell_counts_and_small_dictionaries,
    a_dictionary_shows_at_most_50_values_and_says_how_many_there_are,
    an_empty_catalogue_has_an_empty_summary,
    vector_calls_are_refused_without_the_extension,
);

#[test]
fn the_find_misses_cap_is_100_000() {
    the_cap_is_100_000();
}
