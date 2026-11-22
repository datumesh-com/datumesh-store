//! Store 0.5.3, SP3-SP7 (CONTRACT-round-0.7-P): the bounded reads. Both backends, except the
//! trigram index (PostgreSQL only).
//!
//! Signatures assumed (the contract leaves some types open; the simplest consistent with the
//! existing store types):
//!
//! ```text
//! fn function_test(&self, fn_hash: &str, case: &str) -> Result<Option<FunctionTest>>;
//! fn function_tests_failing(&self, fn_hash: &str, fn_name: Option<&str>, limit: usize)
//!     -> Result<Vec<FunctionTest>>;                      // by case ascending
//! fn false_test_cells(&self, fn_hash: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>>;
//!     // (cell id, bytes of the receipt of the cell's latest version); a cell whose latest
//!     // version is true is not listed. Order not asserted.
//! fn dependants_of(&self, name: &str, at: &str, limit: usize) -> Result<Vec<(String, String)>>;
//!     // (name, fn_hash); order not asserted
//! fn slice_values_page(&self, slice: &Slice, asof: &str, after_cell_id: Option<&str>, limit: usize)
//!     -> Result<Vec<SliceValue>>;                        // by cell id ascending, cell id > after
//! fn catalog_entries_all(&self, limit: usize) -> Result<Vec<(CatalogKind, String, String, String)>>;
//!     // (kind, name, label, description); order not asserted
//! fn dictionary_values_ordered(&self, owner: &str, key: &str, limit: usize) -> Result<Vec<String>>;
//!     // the values' texts, in insertion order
//! ```
//!
//! A test_case cell is `<host>/test_case(case, function, function_hash) -> boolean`: cell
//! `fn` "test_case", an argument named `function_hash`.

#[macro_use]
mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use datumesh_core::json::{self, Value};
use datumesh_core::{ReceiptBuilder, Scalar, Type};
use datumesh_store::{CatalogEntry, CatalogKind, CatalogSource, FunctionTest, Hit, KeyValue};

const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";
const T4: &str = "2024-03-22T00:00:00.000000Z";
const NAME: &str = "rates.example.org/fx_rate";
const OTHER_HOST: &str = "other.example.org";

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

fn case_id(n: u32) -> String {
    format!("{n:016x}")
}

fn test_line(case: &str) -> Vec<u8> {
    json::parse(format!(r#"{{"args":{{"base":{{"type":"string","value":"EUR"}}}},"case":"{case}","expect":"1"}}"#).as_bytes())
        .unwrap()
        .to_jcs()
}

fn test_of(h: &str, n: u32) -> FunctionTest {
    FunctionTest::new(h, &case_id(n), &test_line(&case_id(n)), "rates.example.org")
}

// ---- SP3: function_test, function_tests_failing ------------------------------------------

fn function_test_reads_one_row_by_hash_and_case(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    let (t1, t2) = (test_of(&h, 1), test_of(&h, 2));
    assert!(s.put_function_test(&t1).unwrap());
    assert!(s.put_function_test(&t2).unwrap());
    assert!(s.record_function_test(&h, &case_id(2), T1, "fail", "off by one").unwrap());
    assert_eq!(s.function_test(&h, &case_id(1)).unwrap(), Some(t1));
    let got = s.function_test(&h, &case_id(2)).unwrap().unwrap();
    assert_eq!((got.last_result.as_deref(), got.last_detail.as_deref()), (Some("fail"), Some("off by one")));
    assert_eq!(got.last_run_at.as_deref(), Some(T1));
    assert_eq!(s.function_tests(&h).unwrap()[1], got, "the same row function_tests gives");
    // No such case, no such hash: None, not an error.
    assert_eq!(s.function_test(&h, &case_id(3)).unwrap(), None);
    assert_eq!(s.function_test(&"cd".repeat(32), &case_id(1)).unwrap(), None);
}

fn function_tests_failing_are_fail_or_error_by_case_within_the_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    for n in 1..=8 {
        assert!(s.put_function_test(&test_of(&h, n)).unwrap());
    }
    // 1 pass, 2 fail, 3 error, 4 not run, 5 fail, 6 pass, 7 error, 8 fail.
    for (n, r) in [(1, "pass"), (2, "fail"), (3, "error"), (5, "fail"), (6, "pass"), (7, "error"), (8, "fail")] {
        assert!(s.record_function_test(&h, &case_id(n), T1, r, "d").unwrap());
    }
    // Name 2, 3 and 8.
    let names: Vec<(String, String)> = [2, 3, 8].iter().map(|n| (case_id(*n), NAME.to_string())).collect();
    assert_eq!(s.name_function_tests(&h, &names).unwrap(), 3);
    let cases = |v: Vec<FunctionTest>| v.into_iter().map(|t| t.case).collect::<Vec<_>>();
    let want: Vec<String> = [2, 3, 5, 7, 8].iter().map(|n| case_id(*n)).collect();
    assert_eq!(cases(s.function_tests_failing(&h, None, 100).unwrap()), want);
    for limit in 0..=5 {
        let got = cases(s.function_tests_failing(&h, None, limit).unwrap());
        assert_eq!(got, want[..limit], "limit {limit}: the first by case");
    }
    // Only the named ones.
    let named: Vec<String> = [2, 3, 8].iter().map(|n| case_id(*n)).collect();
    assert_eq!(cases(s.function_tests_failing(&h, Some(NAME), 100).unwrap()), named);
    assert_eq!(cases(s.function_tests_failing(&h, Some(NAME), 2).unwrap()), named[..2]);
    assert!(s.function_tests_failing(&h, Some("rates.example.org/none"), 100).unwrap().is_empty());
    // Whole rows, and another function has none.
    let first = s.function_tests_failing(&h, None, 1).unwrap().remove(0);
    assert_eq!(Some(first), s.function_test(&h, &case_id(2)).unwrap());
    assert!(s.function_tests_failing(&"cd".repeat(32), None, 10).unwrap().is_empty());
    // A failing test that passes on its next run leaves the list.
    assert!(s.record_function_test(&h, &case_id(2), T2, "pass", "fixed").unwrap());
    assert_eq!(cases(s.function_tests_failing(&h, None, 100).unwrap()), want[1..]);
}

// ---- SP4: false_test_cells ----------------------------------------------------------------

fn keydoc_for(host: &str) -> Vec<u8> {
    let k = slice_key();
    let public: String = k.public_key().iter().map(|b| format!("{b:02x}")).collect();
    format!(
        r#"{{"host":"{host}","keys":[{{"alg":"ed25519","id":"{}","public":"{public}","valid_from":"2000-01-01T00:00:00.000000Z"}}],"proto":"datumesh/1"}}"#,
        k.key_id()
    )
    .into_bytes()
}

fn test_case_descriptor() -> Vec<u8> {
    jcs(br#"{"code":{"body":"test_case","lang":"stored"},"inputs":[],"signature":{"args":{"case":{"type":"string","valid":false},"function":{"type":"string","valid":false},"function_hash":{"type":"string","valid":false}},"returns":"boolean"}}"#)
}

fn test_case_cell(host: &str, tc_hash: &str, case: &str, target: &str) -> datumesh_core::Cell {
    let args = Value::Object(vec![
        ("case".into(), Scalar::from_lexical(Type::String, case).unwrap().to_value()),
        ("function".into(), Scalar::from_lexical(Type::String, NAME).unwrap().to_value()),
        ("function_hash".into(), Scalar::from_lexical(Type::String, target).unwrap().to_value()),
    ]);
    let v = Value::Object(vec![
        ("args".into(), args),
        ("fn".into(), Value::String("test_case".into())),
        ("fn_hash".into(), Value::String(tc_hash.into())),
        ("ns".into(), Value::String(host.into())),
    ]);
    datumesh_core::Cell::from_json(&v.to_jcs()).unwrap()
}

/// Stores the boolean receipt of `cell` (of `host`) at `tx`; returns (receipt bytes, hash).
fn put_bool(s: &TStore, host: &str, cell: &datumesh_core::Cell, value: bool, tx: &str) -> Vec<u8> {
    let r = ReceiptBuilder::new()
        .cell(cell.clone())
        .value(Scalar::from_lexical(Type::Boolean, if value { "true" } else { "false" }).unwrap())
        .valid(Default::default())
        .asof(tx)
        .tx(tx)
        .issued(tx)
        .publisher(host)
        .signer_host(host)
        .licence("CC0-1.0")
        .summary("A test case result.")
        .sign(&slice_key())
        .unwrap();
    let bytes = receipt_json(&r);
    let h = s.put_receipt(&bytes).unwrap();
    s.put_version(&cell.id(), tx, &h).unwrap();
    bytes
}

fn false_test_cells_span_namespaces_and_respect_the_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    s.put_namespace(OTHER_HOST, &keydoc_for(OTHER_HOST), VALIDATED_AT).unwrap();
    let tc = s.put_descriptor(&test_case_descriptor()).unwrap();
    let (target, other_target) = ("ab".repeat(32), "cd".repeat(32));

    // Of the function: host A has false(1), false(2), true(3); host B false(4), false(5);
    // case 6 of A is false and then corrected to true (its latest version is true).
    let mut want: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (host, n, v) in [(SLICE_HOST, 1, false), (SLICE_HOST, 2, false), (SLICE_HOST, 3, true), (OTHER_HOST, 4, false), (OTHER_HOST, 5, false)] {
        let c = test_case_cell(host, &tc, &case_id(n), &target);
        let bytes = put_bool(&s, host, &c, v, &day(1));
        if !v {
            want.insert(c.id(), bytes);
        }
    }
    let c6 = test_case_cell(SLICE_HOST, &tc, &case_id(6), &target);
    put_bool(&s, SLICE_HOST, &c6, false, &day(1));
    put_bool(&s, SLICE_HOST, &c6, true, &day(2));
    // Another function's false cell, and a false cell of another function name than test_case
    // (a stored number function with a boolean-looking argument would not be one: skipped here).
    let other = test_case_cell(SLICE_HOST, &tc, &case_id(7), &other_target);
    put_bool(&s, SLICE_HOST, &other, false, &day(1));
    assert_eq!(want.len(), 4);

    let all = s.false_test_cells(&target, 100).unwrap();
    assert_eq!(all.len(), 4, "every false cell of both namespaces, each once");
    assert_eq!(all.iter().cloned().collect::<BTreeMap<_, _>>(), want);
    // Never more than the limit; what is returned is among the wanted, each once.
    for limit in 0..=5 {
        let got = s.false_test_cells(&target, limit).unwrap();
        assert_eq!(got.len(), limit.min(4), "limit {limit}");
        let ids: BTreeSet<&String> = got.iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), got.len());
        for (id, bytes) in &got {
            assert_eq!(want.get(id), Some(bytes));
        }
    }
    assert_eq!(s.false_test_cells(&other_target, 100).unwrap().len(), 1);
    assert!(s.false_test_cells(&"ee".repeat(32), 100).unwrap().is_empty());
}

// ---- SP5: dependants_of -------------------------------------------------------------------

fn reader(body: &str, inputs: &[&str]) -> Vec<u8> {
    let list: Vec<String> = inputs.iter().map(|i| format!("\"{i}\"")).collect();
    jcs(format!(
        r#"{{"code":{{"body":"{body}","lang":"stored"}},"inputs":[{}],"signature":{{"args":{{"base":{{"type":"string","valid":false}}}},"returns":{{"type":"number","unit":"1"}}}}}}"#,
        list.join(",")
    )
    .as_bytes())
}

fn dependants_are_the_names_bound_at_a_time_to_a_reading_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let base = "calc.example.org/base";
    let reads = s.put_descriptor(&reader("reads", &[base])).unwrap();
    let reads2 = s.put_descriptor(&reader("reads too", &["calc.example.org/aaa", base])).unwrap();
    let plain = s.put_descriptor(&reader("plain", &[])).unwrap();
    let other = s.put_descriptor(&reader("other", &["calc.example.org/other"])).unwrap();
    // g1..g4 read base from T1 (g4 through the second hash); g5 reads it from T1 and
    // stops at T3 (rebound to a plain hash); g6 starts reading at T3; p never does; o reads
    // another function.
    s.bind_name("calc.example.org/g1", &reads, T1).unwrap();
    s.bind_name("calc.example.org/g2", &reads, T1).unwrap();
    s.bind_name("calc.example.org/g3", &reads, T1).unwrap();
    s.bind_name("calc.example.org/g4", &reads2, T1).unwrap();
    s.bind_name("calc.example.org/g5", &reads, T1).unwrap();
    s.bind_name("calc.example.org/g5", &plain, T3).unwrap();
    s.bind_name("calc.example.org/g6", &plain, T1).unwrap();
    s.bind_name("calc.example.org/g6", &reads, T3).unwrap();
    s.bind_name("calc.example.org/p", &plain, T1).unwrap();
    s.bind_name("calc.example.org/o", &other, T1).unwrap();

    let set = |at: &str, limit: usize| -> Vec<(String, String)> { s.dependants_of(base, at, limit).unwrap() };
    let names = |v: &[(String, String)]| v.iter().map(|(n, _)| n.clone()).collect::<BTreeSet<_>>();
    let nm = |n: &[&str]| n.iter().map(|x| format!("calc.example.org/{x}")).collect::<BTreeSet<_>>();

    assert!(set("2024-01-01T00:00:00.000000Z", 100).is_empty(), "before any binding");
    let at2 = set(T2, 100);
    assert_eq!(names(&at2), nm(&["g1", "g2", "g3", "g4", "g5"]));
    assert_eq!(at2.len(), 5, "each name once");
    for (n, h) in &at2 {
        let want = if n.ends_with("g4") { &reads2 } else { &reads };
        assert_eq!(h, want, "{n}");
    }
    let at4 = set(T4, 100);
    assert_eq!(names(&at4), nm(&["g1", "g2", "g3", "g4", "g6"]), "g5 no longer reads, g6 does");
    // The limit: at most that many, all among the dependants, each once.
    for limit in 0..=6 {
        let got = set(T2, limit);
        assert_eq!(got.len(), limit.min(5), "limit {limit}");
        assert!(got.iter().all(|p| at2.contains(p)));
        assert_eq!(names(&got).len(), got.len());
    }
    // Nothing reads an unknown name.
    assert!(s.dependants_of("calc.example.org/nobody", T4, 10).unwrap().is_empty());
}

// ---- SP6: text_find with a limit ----------------------------------------------------------

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

fn hit_name(h: &Hit) -> String {
    match h {
        Hit::Entry { entry, .. } => entry.name.clone(),
        Hit::Value { value, .. } => value.clone(),
        other => panic!("unexpected hit {other:?}"),
    }
}

fn hit_score(h: &Hit) -> f64 {
    match h {
        Hit::Entry { score, .. } | Hit::Value { score, .. } => *score,
        other => panic!("unexpected hit {other:?}"),
    }
}

fn text_find_with_more_matches_than_the_limit_gives_exactly_the_limit_best_first(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let mut entries = Vec::new();
    // 40 entries that have the word as a word of the description (0.8), 6 inside a word (0.5),
    // one with the word as its whole label (1.0), 4 that do not match.
    for i in 0..40 {
        entries.push(entry(&format!("t.example.org/word{i:02}"), &format!("Thing {i}"), "a widget thing"));
    }
    for i in 0..6 {
        entries.push(entry(&format!("t.example.org/inside{i}"), &format!("Inside {i}"), "widgetry"));
    }
    entries.push(entry("t.example.org/exact", "widget", "the exact one"));
    for i in 0..4 {
        entries.push(entry(&format!("t.example.org/miss{i}"), "Nothing", "no match here"));
    }
    s.upsert_catalog(&entries).unwrap();

    let all = s.text_find("widget", 1000).unwrap();
    assert_eq!(all.len(), 47);
    assert_eq!(hit_name(&all[0]), "t.example.org/exact");
    assert!(all.windows(2).all(|w| hit_score(&w[0]) >= hit_score(&w[1])), "best first");
    for limit in [1, 5, 10, 46] {
        let got = s.text_find("widget", limit).unwrap();
        assert_eq!(got.len(), limit, "exactly the limit");
        assert_eq!(got.iter().map(hit_name).collect::<Vec<_>>(), all[..limit].iter().map(hit_name).collect::<Vec<_>>(), "limit {limit}: the best ones, in order");
    }
    assert_eq!(s.text_find("widget", 47).unwrap().len(), 47);
    assert_eq!(s.text_find("widget", 500).unwrap().len(), 47);
    assert!(s.text_find("widget", 0).unwrap().is_empty());
    // Two words: the mean score, and the limit still applies after the order.
    let two = s.text_find("widget thing", 3).unwrap();
    assert_eq!(two.len(), 3);
    assert!(two.windows(2).all(|w| hit_score(&w[0]) >= hit_score(&w[1])));
}

fn the_migration_adds_a_trigram_index_on_the_entry_names(b: Backend) {
    if b != Backend::Pg {
        return; // SQLite has no trigram index: its search is in SQL with LIMIT only.
    }
    let db = TestDb::migrated(b);
    let defs = db.texts("SELECT indexdef FROM pg_indexes WHERE tablename = 'catalog_entries'");
    assert!(
        defs.iter().any(|d| d[0].contains("trgm") && d[0].contains("name")),
        "a trigram index on catalog_entries.name: {defs:?}"
    );
}

// ---- SP7: pages ---------------------------------------------------------------------------

fn slice_values_page_covers_every_member_once_by_cell_id(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    // Inserted in reverse key order, so that cell id order is neither of them.
    for k in (1..=25).rev() {
        let c = slice_cell("f", &h, &[("k", sc(Type::Number, &k.to_string()))]);
        put_member(&s, &c, &format!("{k}"), &day(1), &day(1));
    }
    // Three cells get a later version (answers from day 3 on).
    for k in [3, 11, 20] {
        let c = slice_cell("f", &h, &[("k", sc(Type::Number, &k.to_string()))]);
        put_member(&s, &c, "999", &day(3), &day(3));
    }
    let sl = slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#));
    for asof in [day(2), day(5)] {
        let mut want = s.slice_values(&sl, &asof).unwrap();
        assert_eq!(want.len(), 25);
        want.sort_by(|a, b| a.version.cell_id.cmp(&b.version.cell_id));
        for limit in [1usize, 7, 10, 25, 26, 100] {
            let mut got = Vec::new();
            let mut after: Option<String> = None;
            for _ in 0..100 {
                let page = s.slice_values_page(&sl, &asof, after.as_deref(), limit).unwrap();
                assert!(page.len() <= limit, "a page of at most {limit}");
                if page.is_empty() {
                    break;
                }
                after = Some(page.last().unwrap().version.cell_id.clone());
                got.extend(page);
            }
            assert_eq!(got, want, "limit {limit}, asof {asof}: every member exactly once, by cell id");
        }
        // The cursor is exclusive; past the last id nothing.
        let last = want.last().unwrap().version.cell_id.clone();
        assert!(s.slice_values_page(&sl, &asof, Some(&last), 10).unwrap().is_empty());
        let first = want[0].version.cell_id.clone();
        assert_eq!(s.slice_values_page(&sl, &asof, Some(&first), 2).unwrap(), want[1..3]);
        assert!(s.slice_values_page(&sl, &asof, None, 0).unwrap().is_empty());
    }
    // Before any version: nothing.
    assert!(s.slice_values_page(&sl, "2023-01-01T00:00:00.000000Z", None, 10).unwrap().is_empty());
}

fn catalog_entries_all_gives_kind_name_label_description_within_the_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let mut entries = Vec::new();
    for i in 0..9 {
        entries.push(entry(&format!("t.example.org/f{i}"), &format!("Label {i}"), &format!("Description {i}.")));
    }
    for i in 0..3 {
        let mut e = entry(&format!("t.example.org/C{i}"), &format!("Class {i}"), "A class.");
        e.kind = CatalogKind::Class;
        entries.push(e);
    }
    s.upsert_catalog(&entries).unwrap();
    s.upsert_key_values("t.example.org/f0", "k", &[KeyValue { value: "v".into(), label: String::new(), aliases: vec![] }]).unwrap();
    let want: BTreeSet<(String, String, String)> = entries
        .iter()
        .map(|e| (e.name.clone(), e.label.clone(), e.description.clone()))
        .collect();
    let key = |(k, n, l, d): (CatalogKind, String, String, String)| {
        assert_eq!(k == CatalogKind::Class, n.contains("/C"), "{n} has its kind");
        (n, l, d)
    };
    let all: BTreeSet<_> = s.catalog_entries_all(100).unwrap().into_iter().map(key).collect();
    assert_eq!(all, want);
    assert_eq!(s.catalog_entries_all(12).unwrap().len(), 12);
    for limit in [0, 1, 5, 11] {
        let got = s.catalog_entries_all(limit).unwrap();
        assert_eq!(got.len(), limit);
        let got: BTreeSet<_> = got.into_iter().map(key).collect();
        assert!(got.is_subset(&want));
        assert_eq!(got.len(), limit, "no entry twice");
    }
}

fn dictionary_values_ordered_are_in_insertion_order_within_the_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let owner = "scripture.example.org/verse";
    let kv = |v: &str| KeyValue { value: v.to_string(), label: String::new(), aliases: vec![] };
    // Neither alphabetical nor numeric order.
    s.upsert_key_values(owner, "book", &[kv("Zed"), kv("Alpha"), kv("Mid")]).unwrap();
    s.upsert_key_values(owner, "book", &[kv("Beta"), kv("Omega")]).unwrap();
    s.upsert_key_values(owner, "chapter", &[kv("9"), kv("1")]).unwrap();
    // Upserting an existing value again does not move it.
    s.upsert_key_values(owner, "book", &[kv("Alpha")]).unwrap();
    let want = ["Zed", "Alpha", "Mid", "Beta", "Omega"];
    assert_eq!(s.dictionary_values_ordered(owner, "book", 100).unwrap(), want);
    for limit in 0..=6 {
        assert_eq!(s.dictionary_values_ordered(owner, "book", limit).unwrap(), want[..limit.min(5)], "limit {limit}");
    }
    assert_eq!(s.dictionary_values_ordered(owner, "chapter", 100).unwrap(), ["9", "1"]);
    assert!(s.dictionary_values_ordered(owner, "none", 100).unwrap().is_empty());
    assert!(s.dictionary_values_ordered("scripture.example.org/none", "book", 100).unwrap().is_empty());
}

backends!(
    function_test_reads_one_row_by_hash_and_case,
    function_tests_failing_are_fail_or_error_by_case_within_the_limit,
    false_test_cells_span_namespaces_and_respect_the_limit,
    dependants_are_the_names_bound_at_a_time_to_a_reading_hash,
    text_find_with_more_matches_than_the_limit_gives_exactly_the_limit_best_first,
    the_migration_adds_a_trigram_index_on_the_entry_names,
    slice_values_page_covers_every_member_once_by_cell_id,
    catalog_entries_all_gives_kind_name_label_description_within_the_limit,
    dictionary_values_ordered_are_in_insertion_order_within_the_limit,
);
