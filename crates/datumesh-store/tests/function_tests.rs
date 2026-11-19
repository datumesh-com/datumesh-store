//! Function tests, the descriptor inputs index and bound names (0.5.1, migrations 10 on
//! PostgreSQL and 6 on SQLite; CONTRACT-build2-part3-calculus), and dropping a PgStore on a
//! runtime thread (0.5.1). Both backends, except the last test (PostgreSQL's pool).
//!
//! A function test is a canonical test line stored once per (function hash, case): the first
//! writer's row stays. A run records its instant, result and detail on that row. The branch
//! counts are replicated on every row of a function. `function_readers` answers from the
//! descriptors' `inputs` (indexed when a descriptor is stored); `bound_names` from the
//! bindings, every name ever bound to the hash.

#[macro_use]
mod common;

use common::*;
use datumesh_core::json;
use datumesh_store::{Error, FunctionTest, PgStore, Store};

const CASE_A: &str = "00000000000000a1";
const CASE_B: &str = "00000000000000b2";
const CASE_C: &str = "00000000000000c3";
const NAME: &str = "rates.example.org/fx_rate";
const NAME2: &str = "rates.example.org/fx_rate_alias";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";

/// The canonical (JCS) form of a test line.
fn line(text: &str) -> Vec<u8> {
    json::parse(text.as_bytes()).unwrap().to_jcs()
}

/// A test line of `case`, with `expect` as its expected value.
fn test_line(case: &str, expect: &str) -> Vec<u8> {
    line(&format!(r#"{{"args":{{"base":{{"type":"string","value":"EUR"}}}},"case":"{case}","expect":"{expect}"}}"#))
}

/// A stored descriptor of `body` with the given inputs (fully qualified names, ascending).
fn with_inputs(body: &str, inputs: &[&str]) -> Vec<u8> {
    let list: Vec<String> = inputs.iter().map(|i| format!("\"{i}\"")).collect();
    jcs(format!(
        r#"{{"code":{{"body":"{body}","lang":"stored"}},"inputs":[{}],"signature":{{"args":{{"base":{{"type":"string","valid":false}}}},"returns":{{"type":"number","unit":"1"}}}}}}"#,
        list.join(",")
    )
    .as_bytes())
}

/// The function test of `case` of `fn_hash`, written by `by`.
fn test_of(fn_hash: &str, case: &str, expect: &str, by: &str) -> FunctionTest {
    FunctionTest::new(fn_hash, case, &test_line(case, expect), by)
}

fn put_inserts_once_and_keeps_the_first_writer(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let h = fn_hash_of(&fx_rate_descriptor());
    let first = test_of(&h, CASE_A, "1", "rates.example.org");
    assert!(s.put_function_test(&first).unwrap());
    // The same (fn_hash, case): nothing changes, whoever writes it and whatever it says.
    let second = test_of(&h, CASE_A, "2", "calc.example.net");
    assert!(!s.put_function_test(&second).unwrap());
    assert_eq!(s.function_tests(&h).unwrap(), vec![first]);
    assert_eq!(db.count("function_tests"), 1);
    // Another case of the same function is a new row.
    assert!(s.put_function_test(&test_of(&h, CASE_B, "3", "calc.example.net")).unwrap());
    assert_eq!(db.count("function_tests"), 2);
}

fn function_tests_are_ordered_by_case(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    for case in [CASE_C, CASE_A, CASE_B] {
        assert!(s.put_function_test(&test_of(&h, case, "1", "rates.example.org")).unwrap());
    }
    let cases: Vec<String> = s.function_tests(&h).unwrap().into_iter().map(|t| t.case).collect();
    assert_eq!(cases, [CASE_A, CASE_B, CASE_C]);
    // Another function has none of them; nothing is an error.
    assert!(s.function_tests(&"cd".repeat(32)).unwrap().is_empty());
}

fn a_malformed_case_or_hash_is_refused(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    let bad_case = test_of(&h, "not-hex", "1", "rates.example.org");
    assert!(matches!(s.put_function_test(&bad_case), Err(Error::Invalid(_))));
    let bad_hash = test_of("xyz", CASE_A, "1", "rates.example.org");
    assert!(matches!(s.put_function_test(&bad_hash), Err(Error::Invalid(_))));
    assert_eq!(db.count("function_tests"), 0);
}

fn record_sets_the_run_and_refuses_an_unknown_result(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    s.put_function_test(&test_of(&h, CASE_A, "1", "rates.example.org")).unwrap();
    // An existing row: true, and its run fields are set.
    assert!(s.record_function_test(&h, CASE_A, T1, "pass", "matched").unwrap());
    let row = &s.function_tests(&h).unwrap()[0];
    assert_eq!(row.last_run_at.as_deref(), Some(T1));
    assert_eq!(row.last_result.as_deref(), Some("pass"));
    assert_eq!(row.last_detail.as_deref(), Some("matched"));
    assert!(s.record_function_test(&h, CASE_A, T2, "fail", "1 != 2").unwrap());
    assert!(s.record_function_test(&h, CASE_A, T3, "error", "").unwrap());
    let row = &s.function_tests(&h).unwrap()[0];
    assert_eq!(row.last_run_at.as_deref(), Some(T3));
    assert_eq!(row.last_result.as_deref(), Some("error"));
    // A missing row: false, and nothing is inserted.
    assert!(!s.record_function_test(&h, CASE_B, T1, "pass", "").unwrap());
    assert_eq!(db.count("function_tests"), 1);
    // Any other result is an error, and changes nothing.
    assert!(matches!(s.record_function_test(&h, CASE_A, T1, "ok", ""), Err(Error::Invalid(_))));
    assert!(matches!(s.record_function_test(&h, CASE_A, T1, "PASS", ""), Err(Error::Invalid(_))));
    let row = &s.function_tests(&h).unwrap()[0];
    assert_eq!(row.last_result.as_deref(), Some("error"));
    assert_eq!(row.last_run_at.as_deref(), Some(T3));
}

fn branches_are_replicated_on_every_row_of_the_function(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let h = "ab".repeat(32);
    let other = "cd".repeat(32);
    s.put_function_test(&test_of(&h, CASE_A, "1", "rates.example.org")).unwrap();
    s.put_function_test(&test_of(&h, CASE_B, "2", "rates.example.org")).unwrap();
    s.put_function_test(&test_of(&other, CASE_A, "3", "rates.example.org")).unwrap();
    s.set_function_branches(&h, 10, 4).unwrap();
    for t in s.function_tests(&h).unwrap() {
        assert_eq!((t.branches_total, t.branches_covered), (10, 4), "case {}", t.case);
    }
    // The other function is untouched.
    let t = &s.function_tests(&other).unwrap()[0];
    assert_eq!((t.branches_total, t.branches_covered), (0, 0));
    // A function with no rows: no-op, not an error, and no row appears.
    s.set_function_branches(&"ef".repeat(32), 1, 1).unwrap();
    assert!(s.function_tests(&"ef".repeat(32)).unwrap().is_empty());
    // Covered cannot exceed total.
    assert!(matches!(s.set_function_branches(&h, 2, 3), Err(Error::Invalid(_))));
    let t = &s.function_tests(&h).unwrap()[0];
    assert_eq!((t.branches_total, t.branches_covered), (10, 4));
}

fn function_readers_follow_the_descriptors_inputs(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    // A function with inputs, one without, and another with the same input.
    let with = with_inputs("convert", &[NAME, NAME2]);
    let h_with = s.put_descriptor(&with).unwrap();
    let h_plain = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let h_other = s.put_descriptor(&with_inputs("convert v2", &[NAME])).unwrap();
    assert_ne!(h_with, h_plain);
    let mut both = vec![h_with.clone(), h_other.clone()];
    both.sort();
    assert_eq!(s.function_readers(NAME).unwrap(), both);
    assert_eq!(s.function_readers(NAME2).unwrap(), [h_with.clone()]);
    // The function without inputs reads nothing, and nothing reads it by its name.
    assert!(s.function_readers("rates.example.org/nothing").unwrap().is_empty());
    assert!(!s.function_readers(NAME).unwrap().contains(&h_plain));
    // Storing the same descriptor again changes nothing.
    assert_eq!(s.put_descriptor(&with).unwrap(), h_with);
    assert_eq!(s.function_readers(NAME2).unwrap(), [h_with]);
}

fn bound_names_are_every_name_ever_bound_to_the_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let a = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let c = s.put_descriptor(&with_inputs("convert", &[])).unwrap();
    assert!(s.bound_names(&a).unwrap().is_empty());
    s.bind_name(NAME, &a, T1).unwrap();
    s.bind_name(NAME2, &a, T1).unwrap();
    assert_eq!(s.bound_names(&a).unwrap(), [NAME, NAME2]);
    assert!(s.bound_names(&c).unwrap().is_empty());
    // Rebinding NAME to another hash: that hash has it now, and the old one still lists it.
    s.bind_name(NAME, &c, T2).unwrap();
    assert_eq!(s.bound_names(&c).unwrap(), [NAME]);
    assert_eq!(s.bound_names(&a).unwrap(), [NAME, NAME2]);
    // Rebinding it back adds nothing new.
    s.bind_name(NAME, &a, T3).unwrap();
    assert_eq!(s.bound_names(&a).unwrap(), [NAME, NAME2]);
    assert_eq!(s.bound_names(&c).unwrap(), [NAME]);
}

backends!(
    put_inserts_once_and_keeps_the_first_writer,
    function_tests_are_ordered_by_case,
    a_malformed_case_or_hash_is_refused,
    record_sets_the_run_and_refuses_an_unknown_result,
    branches_are_replicated_on_every_row_of_the_function,
    function_readers_follow_the_descriptors_inputs,
    bound_names_are_every_name_ever_bound_to_the_hash,
);

/// A PgStore dropped on a runtime thread (a task of a tokio runtime, and the thread that
/// blocks on it) does not panic: its connections are closed on a thread of their own. The
/// pool holds an idle connection, which is what the drop closes.
#[test]
fn pg_store_drops_on_a_runtime_thread_without_panicking() {
    let db = PgDb::migrated();
    let store: PgStore = db.store();
    store.check_statements().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let task = rt.spawn(async move {
        drop(store);
    });
    rt.block_on(task).expect("dropping a PgStore in a task panicked");
    // And directly on the thread that blocks on the runtime.
    let store = db.store();
    store.check_statements().unwrap();
    rt.block_on(async move {
        drop(store);
    });
}
