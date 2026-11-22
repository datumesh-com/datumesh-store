//! `class_names_bound_to` (CONTRACT-classes-round, the K5 follow-up): the class names bound to a
//! class hash as of an instant. Both backends.
//!
//! The formula and http need it to name D, the class that defines an inherited method, the way
//! `super` names its parent (SPEC 16.3): a class is found by its hash, and its name is a class name
//! bound to that hash as of the request's asof (or the define's instant).
//!
//! API specified by this test. It is ADDITIVE: a new `Store` method with a default body
//! (`Err(Error::Unsupported)`), as 0.5.2's and 0.5.3's were.
//!
//! ```text
//! // on Store:
//! fn class_names_bound_to(&self, class_hash: &str, at: &str) -> Result<Vec<String>>;
//! ```
//!
//! Rules:
//! - the names whose class binding as of `at` (the latest binding with `tx <= at`, the rule of
//!   `resolve_class_binding`) is `class_hash`: ascending (byte order), each once;
//! - a name rebound to another hash is no longer bound to this one from that binding's `tx` on, and
//!   still is at an `at` before it;
//! - a name whose binding as of `at` is a function's is not a class name: never listed;
//! - none bound (the hash is stored or not, or `at` is before the first binding): an empty list, not
//!   an error;
//! - a malformed hash is `E_CANON_HEX`, a malformed instant `E_CANON_INSTANT`.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{ClassEnv, ErrorCode, FunctionDescriptor};

const HOST: &str = "rates.example.org";
const PAIR: &str = "rates.example.org/fx_pair";
const ALIAS: &str = "rates.example.org/fx_alias";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";
const EARLY: &str = "2000-01-01T00:00:00.000000Z";

/// A stored class: its canonical bytes and hash, and its method functions' descriptors.
struct Class {
    bytes: Vec<u8>,
    hash: String,
    functions: Vec<Vec<u8>>,
}

/// A class of two upper_ascii keys and one method `rate`, which `version` tells apart.
fn class(name: &str, version: &str) -> Class {
    let key = r#"{"key_normalisation":"upper_ascii","type":"string","valid":false}"#;
    let rate = jcs(format!(
        r#"{{"code":{{"body":"rate {version}","lang":"stored"}},"inputs":[],"signature":{{"args":{{"base":{key},"date":{{"type":"date","valid":true}},"quote":{key}}},"order":["base","quote","date"],"returns":{{"type":"number","unit":"1"}}}}}}"#
    )
    .as_bytes());
    let text = format!(
        r#"{{"keys":{{"base":{{"type":"string","key_normalisation":"upper_ascii"}},"quote":{{"type":"string","key_normalisation":"upper_ascii"}}}},"methods":{{"rate":"{}"}},"order":["base","quote"],"steward":{{"namespace":"{HOST}","process":"https://{HOST}/classes/change"}}}}"#,
        fn_hash_of(&rate)
    );
    let mut env = ClassEnv::new(HOST);
    env.insert_function(FunctionDescriptor::from_json(&rate).unwrap());
    let flat = datumesh_core::define_class(name, text.as_bytes(), &env).unwrap();
    Class { bytes: flat.canonical(), hash: flat.hash(), functions: vec![rate] }
}

fn store(s: &(impl AsStore + ?Sized), c: &Class) {
    for f in &c.functions {
        s.st().put_descriptor(f).unwrap();
    }
    s.st().put_class(&c.bytes).unwrap();
}

fn names(s: &(impl AsStore + ?Sized), hash: &str, at: &str) -> Vec<String> {
    s.st().class_names_bound_to(hash, at).unwrap()
}

fn none_bound_is_an_empty_list(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = class(PAIR, "v1");
    // Not stored at all.
    assert_eq!(names(&s, &v1.hash, END), Vec::<String>::new());
    // Stored, never bound.
    store(&s, &v1);
    assert_eq!(names(&s, &v1.hash, END), Vec::<String>::new());
    // Bound later than the instant asked.
    s.bind_class(PAIR, &v1.hash, T2).unwrap();
    assert_eq!(names(&s, &v1.hash, T1), Vec::<String>::new());
    assert_eq!(names(&s, &v1.hash, EARLY), Vec::<String>::new());
    // Another hash than the one bound.
    let v2 = class(PAIR, "v2");
    store(&s, &v2);
    assert_eq!(names(&s, &v2.hash, END), Vec::<String>::new());
}

fn a_bound_name_is_listed_from_its_tx_on(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = class(PAIR, "v1");
    store(&s, &v1);
    s.bind_class(PAIR, &v1.hash, T2).unwrap();
    assert_eq!(names(&s, &v1.hash, "2024-03-18T09:59:59.999999Z"), Vec::<String>::new());
    assert_eq!(names(&s, &v1.hash, T2), [PAIR]);
    assert_eq!(names(&s, &v1.hash, T3), [PAIR]);
    assert_eq!(names(&s, &v1.hash, END), [PAIR]);
}

fn a_hash_bound_under_two_names_lists_both_ascending(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = class(PAIR, "v1");
    store(&s, &v1);
    // The later name sorts first, so the order is the names', not the order of binding.
    s.bind_class(PAIR, &v1.hash, T1).unwrap();
    s.bind_class(ALIAS, &v1.hash, T2).unwrap();
    assert_eq!(names(&s, &v1.hash, T1), [PAIR]);
    assert_eq!(names(&s, &v1.hash, T2), [ALIAS, PAIR]);
    assert_eq!(names(&s, &v1.hash, END), [ALIAS, PAIR]);
    // Binding the same hash again under a name already bound lists it once.
    s.bind_class(PAIR, &v1.hash, T3).unwrap();
    assert_eq!(names(&s, &v1.hash, END), [ALIAS, PAIR]);
}

fn a_rebind_away_ends_the_name_s_binding_to_the_hash(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = class(PAIR, "v1");
    let v2 = class(PAIR, "v2");
    store(&s, &v1);
    store(&s, &v2);
    s.bind_class(PAIR, &v1.hash, T1).unwrap();
    s.bind_class(ALIAS, &v1.hash, T2).unwrap();
    // PAIR moves to v2 at T3; ALIAS stays on v1.
    s.bind_class(PAIR, &v2.hash, T3).unwrap();

    assert_eq!(names(&s, &v1.hash, T2), [ALIAS, PAIR]);
    // At T3 PAIR is no longer bound to v1; before T3 it still is.
    assert_eq!(names(&s, &v1.hash, "2024-03-20T08:29:59.999999Z"), [ALIAS, PAIR]);
    assert_eq!(names(&s, &v1.hash, T3), [ALIAS]);
    assert_eq!(names(&s, &v1.hash, END), [ALIAS]);
    assert_eq!(names(&s, &v2.hash, T2), Vec::<String>::new());
    assert_eq!(names(&s, &v2.hash, T3), [PAIR]);
    assert_eq!(names(&s, &v2.hash, END), [PAIR]);
}

fn malformed_arguments_are_the_core_s_codes(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = class(PAIR, "v1");
    assert_eq!(s.class_names_bound_to("abc", END).unwrap_err().code(), Some(ErrorCode::CanonHex));
    assert_eq!(s.class_names_bound_to(&v1.hash.to_uppercase(), END).unwrap_err().code(), Some(ErrorCode::CanonHex));
    assert_eq!(s.class_names_bound_to(&v1.hash, "tomorrow").unwrap_err().code(), Some(ErrorCode::CanonInstant));
}

/// The method names `<class>/<m>` are bound as functions, never as classes: a function name whose
/// binding is a function's does not appear, whatever hash it was bound to.
fn function_names_are_never_listed(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v1 = class(PAIR, "v1");
    store(&s, &v1);
    s.bind_class(PAIR, &v1.hash, T1).unwrap();
    // `PAIR/rate` is bound to the method's function hash by bind_class: not a class name.
    assert_eq!(names(&s, &v1.hash, END), [PAIR]);
    let fn_hash = fn_hash_of(&v1.functions[0]);
    assert_eq!(names(&s, &fn_hash, END), Vec::<String>::new());
}

backends!(
    none_bound_is_an_empty_list,
    a_bound_name_is_listed_from_its_tx_on,
    a_hash_bound_under_two_names_lists_both_ascending,
    a_rebind_away_ends_the_name_s_binding_to_the_hash,
    malformed_arguments_are_the_core_s_codes,
    function_names_are_never_listed,
);
