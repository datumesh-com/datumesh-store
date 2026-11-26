//! Store 0.6.0, R2 (CONTRACT-round-0.7): closed names. Both backends.
//!
//! API assumed (ADDITIVE; a new `Store` method, required or with a default body
//! `Err(Error::Unsupported)` as 0.5.x's were):
//!
//! ```text
//! // on Store:
//! fn close_binding(&self, name: &str, at: &str) -> Result<()>;
//! ```
//!
//! Rules tested:
//! - `close_binding(name, at)` ends the latest binding of a function name or a class name from
//!   `at` on: `resolve_name` / `resolve_binding` / `resolve_class` / `resolve_class_binding`
//!   as of `>= at` give `None`; as of before `at`, the old binding. A name is bound again
//!   later by `bind_name` / `bind_class` with a `tx` later than the close (the new binding's
//!   `tx`, its `since`, is that `tx`). Whether `at` obeys the clock rule is the http layer's job.
//! - Closing a class name also closes its method names (`<class name>/<method>`, which
//!   `bind_class` bound), from the same `at`.
//! - A name with no binding as of `at` (never bound, or already closed) is an error.
//! - The binding audit (migration 7): the close is recorded as one more row of the binding
//!   history with `written_at` filled: `name_bindings` +1 for a function close; for a class
//!   close `class_bindings` +1 and `name_bindings` +1 per method name. The tests read only
//!   those counts and `written_at`, not the schema that represents "closed".
//! - The migration for it is number 14 on PostgreSQL and 10 on SQLite (the latest of each).

#[macro_use]
mod common;

use common::*;
use datumesh_store::migrate;

const NAME: &str = "rates.example.org/fx_rate";
const CLASS: &str = "rates.example.org/fx_pair";
const T1: &str = "2024-03-15T16:00:00.000000Z";
const T1B: &str = "2024-03-15T16:00:00.000001Z";
const T2: &str = "2024-03-18T10:00:00.000000Z";
const BEFORE_T2: &str = "2024-03-18T09:59:59.999999Z";
const T3: &str = "2024-03-20T08:30:00.000000Z";
const T4: &str = "2024-03-22T08:30:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";
const EARLY: &str = "2000-01-01T00:00:00.000000Z";

fn another_fx_rate() -> Vec<u8> {
    stored_descriptor(&[("k", datumesh_core::Type::Number)], "fx rate 2")
}

fn closing_a_function_name_ends_it_from_at_on(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(NAME, &f, T1).unwrap();
    s.close_binding(NAME, T2).unwrap();

    assert_eq!(s.resolve_name(NAME, EARLY).unwrap(), None);
    for a in [T1, T1B, BEFORE_T2] {
        assert_eq!(s.resolve_name(NAME, a).unwrap().as_deref(), Some(f.as_str()), "{a}");
        let bd = s.resolve_binding(NAME, a).unwrap().unwrap();
        assert_eq!((bd.fn_hash.as_str(), bd.tx.as_str()), (f.as_str(), T1));
    }
    for a in [T2, T3, END] {
        assert_eq!(s.resolve_name(NAME, a).unwrap(), None, "{a}");
        assert_eq!(s.resolve_binding(NAME, a).unwrap(), None, "{a}");
    }
    // A function name is not a class name either way.
    assert_eq!(s.resolve_class(NAME, END).unwrap(), None);
}

fn a_closed_function_name_can_be_bound_again_later(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let g = s.put_descriptor(&another_fx_rate()).unwrap();
    s.bind_name(NAME, &f, T1).unwrap();
    s.close_binding(NAME, T2).unwrap();
    s.bind_name(NAME, &g, T3).unwrap();

    assert_eq!(s.resolve_name(NAME, BEFORE_T2).unwrap().as_deref(), Some(f.as_str()));
    assert_eq!(s.resolve_name(NAME, T2).unwrap(), None);
    assert_eq!(s.resolve_name(NAME, "2024-03-20T08:29:59.999999Z").unwrap(), None);
    let bd = s.resolve_binding(NAME, T3).unwrap().unwrap();
    assert_eq!((bd.fn_hash.as_str(), bd.tx.as_str()), (g.as_str(), T3));
    assert_eq!(s.resolve_name(NAME, END).unwrap().as_deref(), Some(g.as_str()));
    // The same function again after a second close.
    s.close_binding(NAME, T4).unwrap();
    assert_eq!(s.resolve_name(NAME, END).unwrap(), None);
    assert_eq!(s.resolve_name(NAME, "2024-03-21T00:00:00.000000Z").unwrap().as_deref(), Some(g.as_str()));
}

fn closing_what_is_not_bound_is_an_error(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    // Never bound.
    assert!(s.close_binding(NAME, T2).is_err());
    assert!(s.close_binding(CLASS, T2).is_err());
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(NAME, &f, T1).unwrap();
    s.close_binding(NAME, T2).unwrap();
    // Already closed.
    assert!(s.close_binding(NAME, T3).is_err());
    // Still closed as before: the failed close changed nothing.
    assert_eq!(s.resolve_name(NAME, BEFORE_T2).unwrap().as_deref(), Some(f.as_str()));
    assert_eq!(s.resolve_name(NAME, END).unwrap(), None);
    // A malformed instant is E_CANON_INSTANT, as for the other binding calls.
    let g = s.put_descriptor(&another_fx_rate()).unwrap();
    s.bind_name("rates.example.org/other", &g, T1).unwrap();
    let e = s.close_binding("rates.example.org/other", "2024-03-18").unwrap_err();
    assert!(
        matches!(&e, datumesh_store::Error::Invalid(c) if c.code() == datumesh_core::ErrorCode::CanonInstant),
        "{e:?}"
    );
}

fn closing_one_name_leaves_the_others(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let g = s.put_descriptor(&another_fx_rate()).unwrap();
    let other = "rates.example.org/fx_other";
    s.bind_name(NAME, &f, T1).unwrap();
    s.bind_name(other, &g, T1).unwrap();
    s.close_binding(NAME, T2).unwrap();
    assert_eq!(s.resolve_name(other, END).unwrap().as_deref(), Some(g.as_str()));
    assert_eq!(s.resolve_name(NAME, END).unwrap(), None);
}

fn closing_a_class_name_closes_its_method_names(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let c = test_class();
    let h = put_test_class(&s, &c);
    s.bind_class(CLASS, &h, T1).unwrap();
    let methods = [format!("{CLASS}/rate"), format!("{CLASS}/spot")];
    let fns: Vec<String> =
        methods.iter().map(|m| s.resolve_name(m, T1).unwrap().expect("method bound by bind_class")).collect();
    s.close_binding(CLASS, T2).unwrap();

    // Before: all there.
    assert_eq!(s.resolve_class(CLASS, BEFORE_T2).unwrap().as_deref(), Some(h.as_str()));
    let cb = s.resolve_class_binding(CLASS, BEFORE_T2).unwrap().unwrap();
    assert_eq!((cb.class_hash.as_str(), cb.tx.as_str()), (h.as_str(), T1));
    assert_eq!(s.class_names_bound_to(&h, BEFORE_T2).unwrap(), [CLASS]);
    for (m, f) in methods.iter().zip(&fns) {
        assert_eq!(s.resolve_name(m, BEFORE_T2).unwrap().as_ref(), Some(f));
    }
    // From the close on: none.
    for a in [T2, T3, END] {
        assert_eq!(s.resolve_class(CLASS, a).unwrap(), None, "{a}");
        assert_eq!(s.resolve_class_binding(CLASS, a).unwrap(), None, "{a}");
        assert!(s.class_names_bound_to(&h, a).unwrap().is_empty(), "{a}");
        for m in &methods {
            assert_eq!(s.resolve_name(m, a).unwrap(), None, "{m} {a}");
            assert_eq!(s.resolve_binding(m, a).unwrap(), None, "{m} {a}");
        }
    }
    // The class itself stays stored.
    assert_eq!(s.class(&h).unwrap().as_deref(), Some(c.bytes.as_slice()));
}

fn a_closed_class_name_can_be_bound_again_with_new_since(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let c = test_class();
    let h = put_test_class(&s, &c);
    s.bind_class(CLASS, &h, T1).unwrap();
    s.close_binding(CLASS, T2).unwrap();
    s.bind_class(CLASS, &h, T3).unwrap();
    let cb = s.resolve_class_binding(CLASS, END).unwrap().unwrap();
    assert_eq!((cb.class_hash.as_str(), cb.tx.as_str()), (h.as_str(), T3));
    assert_eq!(s.resolve_class(CLASS, T2).unwrap(), None);
    assert_eq!(s.resolve_class(CLASS, BEFORE_T2).unwrap().as_deref(), Some(h.as_str()));
    let m = format!("{CLASS}/rate");
    let bd = s.resolve_binding(&m, END).unwrap().unwrap();
    assert_eq!(bd.tx, T3);
    assert_eq!(s.resolve_name(&m, T2).unwrap(), None);
    assert!(s.resolve_name(&m, BEFORE_T2).unwrap().is_some());
}

fn the_audit_records_the_close(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let f = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name(NAME, &f, T1).unwrap();
    let c = test_class();
    let h = put_test_class(&s, &c);
    s.bind_class(CLASS, &h, T1).unwrap();
    let (names0, classes0) = (db.count("name_bindings"), db.count("class_bindings"));

    s.close_binding(NAME, T2).unwrap();
    assert_eq!(db.count("name_bindings"), names0 + 1);
    assert_eq!(db.count("class_bindings"), classes0);

    s.close_binding(CLASS, T2).unwrap();
    assert_eq!(db.count("class_bindings"), classes0 + 1);
    assert_eq!(db.count("name_bindings"), names0 + 1 + 2, "one close per method name (rate, spot)");

    assert_eq!(db.count("name_bindings"), db.int("SELECT count(written_at) FROM name_bindings"));
    assert_eq!(db.count("class_bindings"), db.int("SELECT count(written_at) FROM class_bindings"));
}

fn the_latest_migrations_are_14_and_10(b: Backend) {
    let last = match b {
        Backend::Pg => migrate::MIGRATIONS.last().unwrap().version,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.last().unwrap().version,
    };
    assert_eq!(last, if b == Backend::Pg { 16 } else { 12 });
}

backends!(
    closing_a_function_name_ends_it_from_at_on,
    a_closed_function_name_can_be_bound_again_later,
    closing_what_is_not_bound_is_an_error,
    closing_one_name_leaves_the_others,
    closing_a_class_name_closes_its_method_names,
    a_closed_class_name_can_be_bound_again_with_new_since,
    the_audit_records_the_close,
    the_latest_migrations_are_14_and_10,
);
