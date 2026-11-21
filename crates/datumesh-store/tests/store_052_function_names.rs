//! Store 0.5.2, S1-S3 (CONTRACT-0.4.9): the function name of a test (`name_function_tests`,
//! `unnamed_function_tests`), the one-query summary (`function_test_summary`) and the page of
//! tests (`function_tests_page`). Both backends.
//!
//! Choices:
//! - `TestSummary` is exported at the crate root (`datumesh_store::TestSummary`); its fields are
//!   `count`, `pass`, `fail`, `error`, `not_run` (u64), `last_run_at: Option<String>`,
//!   `branches_total`, `branches_covered` (u32). It is `#[non_exhaustive]`, so the tests read
//!   its fields one by one (via `Debug`-free tuples) and never construct it; `Default` is used
//!   for the empty case, compared field by field (no `PartialEq` assumed).
//! - `not_run` counts rows whose `last_result` is NULL; `last_run_at` is the maximum of the
//!   rows' `last_run_at` (instants compare as text). `branches_*` are the values replicated on
//!   the rows (`set_function_branches`), 0 when no row matches.
//! - `name_function_tests` ignores cases that have no row (they change nothing and count 0);
//!   an empty `names` slice returns 0.
//! - `unnamed_function_tests(limit)` orders by (fn_hash, case), both ascending, as text;
//!   `limit` 0 returns an empty list.
//! - `function_tests_page` with `fn_name = None` returns the rows of the hash named or not;
//!   `after_case` is exclusive; `limit` 0 returns an empty list.
//! - A function name in these tests is a fully qualified name (`host/path`).
//! - Cells and receipts for S4-S6 use the `slices.example.org` helpers of `common`.

#[macro_use]
mod common;

use common::*;
use datumesh_core::json;
use datumesh_store::{FunctionTest, TestSummary};

const NAME_A: &str = "rates.example.org/fx_rate";
const NAME_B: &str = "rates.example.org/fx_rate_alias";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";

fn case(n: u32) -> String {
    format!("{n:016x}")
}

fn hash(c: char) -> String {
    c.to_string().repeat(64)
}

fn line(c: &str) -> Vec<u8> {
    json::parse(format!(r#"{{"args":{{"base":{{"type":"string","value":"EUR"}}}},"case":"{c}","expect":"1"}}"#).as_bytes())
        .unwrap()
        .to_jcs()
}

fn put(s: &TStore, h: &str, n: u32) {
    let c = case(n);
    assert!(s.put_function_test(&FunctionTest::new(h, &c, &line(&c), "rates.example.org")).unwrap());
}

fn names(list: &[(u32, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(n, name)| (case(*n), name.to_string())).collect()
}

fn cases(v: &[FunctionTest]) -> Vec<String> {
    v.iter().map(|t| t.case.clone()).collect()
}

type Sum = (u64, u64, u64, u64, u64, Option<String>, u32, u32);

fn sum(t: &TestSummary) -> Sum {
    (t.count, t.pass, t.fail, t.error, t.not_run, t.last_run_at.clone(), t.branches_total, t.branches_covered)
}

fn name_function_tests_sets_null_names_only_and_counts_changes(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = hash('a');
    for n in 1..=4 {
        put(&s, &h, n);
    }
    // Two rows named; the count is the rows changed.
    assert_eq!(s.name_function_tests(&h, &names(&[(1, NAME_A), (2, NAME_A)])).unwrap(), 2);
    // A named row keeps its name, an unnamed one is named: only that one changes.
    assert_eq!(s.name_function_tests(&h, &names(&[(1, NAME_B), (3, NAME_B)])).unwrap(), 1);
    // The same call again changes nothing.
    assert_eq!(s.name_function_tests(&h, &names(&[(1, NAME_B), (3, NAME_B)])).unwrap(), 0);
    // A case with no row changes nothing; an empty list is 0.
    assert_eq!(s.name_function_tests(&h, &names(&[(9, NAME_A)])).unwrap(), 0);
    assert_eq!(s.name_function_tests(&h, &[]).unwrap(), 0);
    // Another hash's rows are never touched, even for the same case.
    let other = hash('b');
    put(&s, &other, 4);
    assert_eq!(s.name_function_tests(&h, &names(&[(4, NAME_A)])).unwrap(), 1);
    assert_eq!(s.function_tests_page(&other, Some(NAME_A), None, 10).unwrap().len(), 0);
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_A), None, 10).unwrap()), [case(1), case(2), case(4)]);
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_B), None, 10).unwrap()), [case(3)]);
    // Names are never rewritten: case 1 is still NAME_A's, not NAME_B's.
    assert!(s.function_tests_page(&h, Some(NAME_B), None, 10).unwrap().iter().all(|t| t.case != case(1)));
    assert_eq!(cases(&s.function_tests_page(&h, None, None, 10).unwrap()), [case(1), case(2), case(3), case(4)]);
}

fn a_name_once_set_is_never_overwritten(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = hash('a');
    put(&s, &h, 1);
    assert_eq!(s.name_function_tests(&h, &names(&[(1, NAME_A)])).unwrap(), 1);
    assert_eq!(s.name_function_tests(&h, &names(&[(1, NAME_B)])).unwrap(), 0);
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_A), None, 10).unwrap()), [case(1)]);
    assert!(s.function_tests_page(&h, Some(NAME_B), None, 10).unwrap().is_empty());
    // Running the test or setting branches does not unname it.
    s.record_function_test(&h, &case(1), T1, "pass", "").unwrap();
    s.set_function_branches(&h, 3, 1).unwrap();
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_A), None, 10).unwrap()), [case(1)]);
    assert!(s.unnamed_function_tests(10).unwrap().is_empty());
}

fn unnamed_function_tests_are_ordered_by_hash_and_case_with_limit(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let (h1, h2) = (hash('a'), hash('b'));
    // Inserted out of order, across two hashes.
    for (h, n) in [(&h2, 1), (&h1, 3), (&h2, 0), (&h1, 1), (&h1, 2)] {
        put(&s, h, n);
    }
    let all = s.unnamed_function_tests(100).unwrap();
    let got: Vec<(String, String)> = all.iter().map(|t| (t.fn_hash.clone(), t.case.clone())).collect();
    let want: Vec<(String, String)> =
        vec![(h1.clone(), case(1)), (h1.clone(), case(2)), (h1.clone(), case(3)), (h2.clone(), case(0)), (h2.clone(), case(1))];
    assert_eq!(got, want);
    // The rows are whole rows.
    assert_eq!(all[0], FunctionTest::new(&h1, &case(1), &line(&case(1)), "rates.example.org"));
    // A limit is a prefix.
    let two: Vec<(String, String)> =
        s.unnamed_function_tests(2).unwrap().iter().map(|t| (t.fn_hash.clone(), t.case.clone())).collect();
    assert_eq!(two, want[..2]);
    assert!(s.unnamed_function_tests(0).unwrap().is_empty());
    // Naming a row removes it from the list; the rest keep their order.
    assert_eq!(s.name_function_tests(&h1, &names(&[(2, NAME_A)])).unwrap(), 1);
    let got: Vec<(String, String)> =
        s.unnamed_function_tests(100).unwrap().iter().map(|t| (t.fn_hash.clone(), t.case.clone())).collect();
    let mut want2 = want.clone();
    want2.remove(1);
    assert_eq!(got, want2);
    // Batches: naming what a batch returned drains the list.
    loop {
        let batch = s.unnamed_function_tests(2).unwrap();
        if batch.is_empty() {
            break;
        }
        for t in &batch {
            assert_eq!(s.name_function_tests(&t.fn_hash, &[(t.case.clone(), NAME_B.to_string())]).unwrap(), 1);
        }
    }
    assert!(s.unnamed_function_tests(100).unwrap().is_empty());
}

/// Six tests of `h`: cases 1-3 named A (pass at T1, fail at T3, never run), cases 4-5 named B
/// (error at T2, pass at T1), case 6 unnamed (pass at T2); branches 10/4.
fn summary_fixture(s: &TStore, h: &str) {
    for n in 1..=6 {
        put(s, h, n);
    }
    s.name_function_tests(h, &names(&[(1, NAME_A), (2, NAME_A), (3, NAME_A), (4, NAME_B), (5, NAME_B)])).unwrap();
    s.record_function_test(h, &case(1), T1, "pass", "").unwrap();
    s.record_function_test(h, &case(2), T3, "fail", "x").unwrap();
    s.record_function_test(h, &case(4), T2, "error", "boom").unwrap();
    s.record_function_test(h, &case(5), T1, "pass", "").unwrap();
    s.record_function_test(h, &case(6), T2, "pass", "").unwrap();
    s.set_function_branches(h, 10, 4).unwrap();
}

fn the_summary_counts_results_and_takes_the_latest_run(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = hash('a');
    summary_fixture(&s, &h);
    // None: every row of the hash, named or not.
    assert_eq!(sum(&s.function_test_summary(&h, None).unwrap()), (6, 3, 1, 1, 1, Some(T3.to_string()), 10, 4));
    // Some(name): only that name's rows.
    assert_eq!(sum(&s.function_test_summary(&h, Some(NAME_A)).unwrap()), (3, 1, 1, 0, 1, Some(T3.to_string()), 10, 4));
    assert_eq!(sum(&s.function_test_summary(&h, Some(NAME_B)).unwrap()), (2, 1, 0, 1, 0, Some(T2.to_string()), 10, 4));
    // A name no row has.
    let none = s.function_test_summary(&h, Some("rates.example.org/other")).unwrap();
    assert_eq!((none.count, none.pass, none.fail, none.error, none.not_run), (0, 0, 0, 0, 0));
    assert_eq!(none.last_run_at, None);
    // Another hash is not mixed in.
    let other = hash('b');
    put(&s, &other, 1);
    assert_eq!(sum(&s.function_test_summary(&other, None).unwrap()), (1, 0, 0, 0, 1, None, 0, 0));
    assert_eq!(s.function_test_summary(&h, None).unwrap().count, 6);
}

fn a_summary_of_unrun_tests_has_no_last_run(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = hash('a');
    for n in 1..=3 {
        put(&s, &h, n);
    }
    assert_eq!(sum(&s.function_test_summary(&h, None).unwrap()), (3, 0, 0, 0, 3, None, 0, 0));
}

fn the_summary_of_an_empty_hash_is_all_zeros(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let d = TestSummary::default();
    assert_eq!(sum(&d), (0, 0, 0, 0, 0, None, 0, 0));
    assert_eq!(sum(&s.function_test_summary(&hash('c'), None).unwrap()), sum(&d));
    assert_eq!(sum(&s.function_test_summary(&hash('c'), Some(NAME_A)).unwrap()), sum(&d));
}

fn the_page_is_ordered_by_case_with_after_case_limit_and_name(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = hash('a');
    // Inserted out of order.
    for n in [5, 2, 6, 1, 4, 3] {
        put(&s, &h, n);
    }
    s.name_function_tests(&h, &names(&[(1, NAME_A), (3, NAME_A), (5, NAME_A), (2, NAME_B), (4, NAME_B)])).unwrap();
    let all = [case(1), case(2), case(3), case(4), case(5), case(6)];
    assert_eq!(cases(&s.function_tests_page(&h, None, None, 100).unwrap()), all);
    // Limit.
    assert_eq!(cases(&s.function_tests_page(&h, None, None, 4).unwrap()), all[..4]);
    assert!(s.function_tests_page(&h, None, None, 0).unwrap().is_empty());
    // after_case is exclusive, and need not be a case of the hash.
    assert_eq!(cases(&s.function_tests_page(&h, None, Some(&case(2)), 100).unwrap()), all[2..]);
    assert_eq!(cases(&s.function_tests_page(&h, None, Some(&case(2)), 2).unwrap()), all[2..4]);
    assert_eq!(cases(&s.function_tests_page(&h, None, Some(&case(6)), 100).unwrap()), Vec::<String>::new());
    assert_eq!(cases(&s.function_tests_page(&h, None, Some("0000000000000000"), 100).unwrap()), all);
    // Name filter; a page of it and its continuation.
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_A), None, 100).unwrap()), [case(1), case(3), case(5)]);
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_A), None, 2).unwrap()), [case(1), case(3)]);
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_A), Some(&case(3)), 2).unwrap()), [case(5)]);
    assert_eq!(cases(&s.function_tests_page(&h, Some(NAME_B), Some(&case(2)), 100).unwrap()), [case(4)]);
    // Whole rows, as `function_tests` gives them.
    let full = s.function_tests(&h).unwrap();
    assert_eq!(s.function_tests_page(&h, None, None, 100).unwrap(), full);
    // Another hash: nothing.
    assert!(s.function_tests_page(&hash('b'), None, None, 10).unwrap().is_empty());
}

backends!(
    name_function_tests_sets_null_names_only_and_counts_changes,
    a_name_once_set_is_never_overwritten,
    unnamed_function_tests_are_ordered_by_hash_and_case_with_limit,
    the_summary_counts_results_and_takes_the_latest_run,
    a_summary_of_unrun_tests_has_no_last_run,
    the_summary_of_an_empty_hash_is_all_zeros,
    the_page_is_ordered_by_case_with_after_case_limit_and_name,
);
