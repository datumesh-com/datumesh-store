//! `slice_members` and `versions_between` (0.3.1, SPEC 15.13).
//!
//! * `slice_members(slice, A)`: the cells of the slice's function (its hash, `ns` and `fn`,
//!   the same argument names) whose arguments match (bound arguments the same typed scalar,
//!   ranged ones of the range's type and within it in the type's order, any value for an
//!   open one) and that have a stored version answering as of A (`tx <= A` and the
//!   receipt's `asof <= A`, as `stored_asof`), each with that version and its receipt's
//!   bytes, in key order: the open and ranged arguments in ascending name order, each in its
//!   type's order (numbers numerically, dates and instants chronologically, strings by code
//!   point, `false` before `true`).
//! * `versions_between(cell, from, to, A)`: the stored versions with `tx` in `[from, to]`
//!   answering as of A, ascending by tx, with their receipts' bytes (a version slice).

mod common;

use common::*;
use datumesh_core::{ErrorCode, Receipt, Resolver, Type};
use datumesh_store::{PgStore, SliceMember, Store};

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

const END: &str = "9999-12-31T23:59:59.999999Z";

/// The lexical value of argument `name` of each member's cell.
fn arg_of(ms: &[SliceMember], name: &str) -> Vec<String> {
    ms.iter()
        .map(|m| {
            let r = Receipt::from_json(&m.receipt).unwrap();
            r.cell().args.get(name).and_then(|a| a.scalar()).unwrap().lexical().to_string()
        })
        .collect()
}

/// Each member's version, receipt and cell agree with what the store holds.
fn check_members(s: &PgStore, ms: &[SliceMember], asof: &str) {
    for m in ms {
        let r = Receipt::from_json(&m.receipt).unwrap();
        assert_eq!(r.hash(), m.version.receipt_hash);
        assert_eq!(r.cell().id(), m.version.cell_id);
        assert_eq!(r.tx(), m.version.tx);
        assert_eq!(r.asof(), m.version.asof);
        assert_eq!(&*s.receipt(r.hash()).unwrap(), m.receipt.as_slice());
        assert_eq!(s.stored_asof(&m.version.cell_id, asof).unwrap().as_ref(), Some(&m.version));
    }
}

/// A function of `SLICE_HOST` with one argument `k` of type `t`, and a cell of it per value
/// (each with one version at day 1).
fn one_arg(s: &PgStore, t: Type, values: &[&str]) -> String {
    let h = s.put_descriptor(&stored_descriptor(&[("k", t)], t.as_str())).unwrap();
    for (i, v) in values.iter().enumerate() {
        put_member(s, &slice_cell("f", &h, &[("k", sc(t, v))]), &format!("{i}"), &day(1), &day(1));
    }
    h
}

fn open(h: &str) -> datumesh_core::Slice {
    slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#))
}

fn range(h: &str, t: &str, lo: &str, hi: &str) -> datumesh_core::Slice {
    slice(&format!(
        r#"{{"args":{{"k":{{"hi":"{hi}","lo":"{lo}","type":"{t}"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
    ))
}

#[test]
fn numbers_are_ordered_and_ranged_as_decimals() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let h = one_arg(&s, Type::Number, &["10", "9", "-1", "-1.5", "0", "0.001", "9.5", "100", "-10", "10.5", "1"]);
    let all = s.slice_members(&open(&h), &day(2)).unwrap();
    assert_eq!(arg_of(&all, "k"), ["-10", "-1.5", "-1", "0", "0.001", "1", "9", "9.5", "10", "10.5", "100"]);
    check_members(&s, &all, &day(2));
    let at = |lo: &str, hi: &str| arg_of(&s.slice_members(&range(&h, "number", lo, hi), &day(2)).unwrap(), "k");
    assert_eq!(at("9", "10.5"), ["9", "9.5", "10", "10.5"]);
    assert_eq!(at("9.25", "10"), ["9.5", "10"]);
    assert_eq!(at("-1.5", "-1"), ["-1.5", "-1"]);
    assert_eq!(at("-2", "0"), ["-1.5", "-1", "0"]);
    assert_eq!(at("0.0001", "0.01"), ["0.001"]);
    assert_eq!(at("100", "100"), ["100"]);
    assert_eq!(at("11", "99"), Vec::<String>::new());
    // lo after hi is an empty range, not an error.
    assert_eq!(at("10", "9"), Vec::<String>::new());
    // A range of another type holds none of them.
    assert!(s.slice_members(&range(&h, "string", "0", "9"), &day(2)).unwrap().is_empty());
    // A bound argument: the same typed scalar.
    let bound = slice(&format!(r#"{{"args":{{"k":{{"type":"number","value":"9.5"}},"x":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#));
    // (No cell has an argument x, so none has the slice's argument names.)
    assert!(s.slice_members(&bound, &day(2)).unwrap().is_empty());
}

#[test]
fn dates_instants_and_strings_in_their_order() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let d = one_arg(&s, Type::Date, &["2024-02-29", "0001-01-01", "9999-12-31", "2023-12-31", "2024-01-01"]);
    let got = arg_of(&s.slice_members(&open(&d), &day(2)).unwrap(), "k");
    assert_eq!(got, ["0001-01-01", "2023-12-31", "2024-01-01", "2024-02-29", "9999-12-31"]);
    let got = arg_of(&s.slice_members(&range(&d, "date", "2023-12-31", "2024-02-28"), &day(2)).unwrap(), "k");
    assert_eq!(got, ["2023-12-31", "2024-01-01"]);

    let i = one_arg(
        &s,
        Type::Instant,
        &["2024-03-15T16:00:00.000001Z", "2024-03-15T16:00:00.000000Z", "1999-12-31T23:59:59.999999Z", END],
    );
    let got = arg_of(&s.slice_members(&open(&i), &day(2)).unwrap(), "k");
    assert_eq!(got, ["1999-12-31T23:59:59.999999Z", "2024-03-15T16:00:00.000000Z", "2024-03-15T16:00:00.000001Z", END]);
    let got = arg_of(
        &s.slice_members(&range(&i, "instant", "2024-03-15T16:00:00.000001Z", END), &day(2)).unwrap(),
        "k",
    );
    assert_eq!(got, ["2024-03-15T16:00:00.000001Z", END]);

    // By code point: U+FF61 before U+1F600 (UTF-16 code units would put the emoji first),
    // "Z" before "a", "a" before "ab", "é" (U+00E9) after "z".
    let st = one_arg(&s, Type::String, &["\u{1F600}", "\u{FF61}", "ab", "a", "Z", "z", "é", ""]);
    let got = arg_of(&s.slice_members(&open(&st), &day(2)).unwrap(), "k");
    assert_eq!(got, ["", "Z", "a", "ab", "z", "é", "\u{FF61}", "\u{1F600}"]);
    let got = arg_of(&s.slice_members(&range(&st, "string", "a", "z"), &day(2)).unwrap(), "k");
    assert_eq!(got, ["a", "ab", "z"]);
    let got = arg_of(&s.slice_members(&range(&st, "string", "\u{FF00}", "\u{10FFFF}"), &day(2)).unwrap(), "k");
    assert_eq!(got, ["\u{FF61}", "\u{1F600}"]);
}

#[test]
fn bound_open_and_ranged_arguments_in_key_order() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let args = [("a", Type::String), ("b", Type::Number), ("c", Type::Date), ("d", Type::Boolean)];
    let h = s.put_descriptor(&stored_descriptor(&args, "g")).unwrap();
    let mut n = 0;
    for a in ["x", "y"] {
        for b in ["10", "9", "-0.5"] {
            for c in ["2024-01-02", "2024-01-01", "2023-06-30"] {
                for d in ["true", "false"] {
                    let cell = slice_cell(
                        "g",
                        &h,
                        &[("a", sc(Type::String, a)), ("b", sc(Type::Number, b)), ("c", sc(Type::Date, c)), ("d", sc(Type::Boolean, d))],
                    );
                    put_member(&s, &cell, &format!("{n}"), &day(1), &day(1));
                    n += 1;
                }
            }
        }
    }
    let text = |args: &str| format!(r#"{{"args":{{{args}}},"fn":"g","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#);
    // a bound, b open, c ranged, d open: key (b, c, d).
    let sl = slice(&text(
        r#""a":{"type":"string","value":"x"},"b":"*","c":{"hi":"2024-01-01","lo":"2023-07-01","type":"date"},"d":"*""#,
    ));
    let ms = s.slice_members(&sl, &day(5)).unwrap();
    check_members(&s, &ms, &day(5));
    let keys: Vec<(String, String, String)> = arg_of(&ms, "b")
        .into_iter()
        .zip(arg_of(&ms, "c"))
        .zip(arg_of(&ms, "d"))
        .map(|((b, c), d)| (b, c, d))
        .collect();
    let want: Vec<(String, String, String)> = ["-0.5", "9", "10"]
        .iter()
        .flat_map(|b| ["false", "true"].iter().map(move |d| (b.to_string(), "2024-01-01".to_string(), d.to_string())))
        .collect();
    assert_eq!(keys, want);
    assert!(arg_of(&ms, "a").iter().all(|a| a == "x"));
    // Every argument open: 36 members, key (a, b, c, d).
    let ms = s.slice_members(&slice(&text(r#""a":"*","b":"*","c":"*","d":"*""#)), &day(5)).unwrap();
    assert_eq!(ms.len(), 36);
    let first = (arg_of(&ms[..1], "a"), arg_of(&ms[..1], "b"), arg_of(&ms[..1], "c"), arg_of(&ms[..1], "d"));
    assert_eq!(first, (vec!["x".into()], vec!["-0.5".into()], vec!["2023-06-30".into()], vec!["false".into()]));
    // A boolean bound: half of them.
    let ms = s.slice_members(&slice(&text(r#""a":"*","b":"*","c":"*","d":{"type":"boolean","value":true}"#)), &day(5)).unwrap();
    assert_eq!(ms.len(), 18);
    assert!(arg_of(&ms, "d").iter().all(|d| d == "true"));
    // A bound value of another type matches nothing.
    let ms = s.slice_members(&slice(&text(r#""a":"*","b":{"type":"string","value":"9"},"c":"*","d":"*""#)), &day(5)).unwrap();
    assert!(ms.is_empty());
}

#[test]
fn only_cells_of_the_slices_function_and_arguments() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let other = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "other")).unwrap();
    put_member(&s, &slice_cell("f", &h, &[("k", sc(Type::Number, "1"))]), "1", &day(1), &day(1));
    // Another hash, another name with this hash, and a cell of this hash with other arguments
    // (signature-valid, whatever its descriptor says): none is a member.
    put_member(&s, &slice_cell("f", &other, &[("k", sc(Type::Number, "2"))]), "2", &day(1), &day(1));
    put_member(&s, &slice_cell("f2", &h, &[("k", sc(Type::Number, "3"))]), "3", &day(1), &day(1));
    put_member(
        &s,
        &slice_cell("f", &h, &[("k", sc(Type::Number, "4")), ("z", sc(Type::Number, "4"))]),
        "4",
        &day(1),
        &day(1),
    );
    let ms = s.slice_members(&open(&h), &day(2)).unwrap();
    assert_eq!(arg_of(&ms, "k"), ["1"]);
    // A receipt stored without a version is not a member either.
    let c5 = slice_cell("f", &h, &[("k", sc(Type::Number, "5"))]);
    s.put_receipt(&slice_receipt(&c5, "5", &day(1), &day(1))).unwrap();
    assert_eq!(arg_of(&s.slice_members(&open(&h), &day(2)).unwrap(), "k"), ["1"]);
    // Computed versions are not stored versions.
    let r5 = Receipt::from_json(&slice_receipt(&c5, "5", &day(1), &day(1))).unwrap();
    s.put_computed(&c5.id(), &day(1), r5.hash()).unwrap();
    assert_eq!(arg_of(&s.slice_members(&open(&h), &day(2)).unwrap(), "k"), ["1"]);
}

#[test]
fn members_answer_as_of_the_asof() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let cell = |k: &str| slice_cell("f", &h, &[("k", sc(Type::Number, k))]);
    // k=1: v1 at day 2; a correction at day 6. k=2: late-signed (tx day 3, asof day 8).
    // k=3: first version at day 10.
    let a1 = put_member(&s, &cell("1"), "100", &day(2), &day(2));
    let a2 = put_member(&s, &cell("1"), "101", &day(6), &day(6));
    let b1 = put_member(&s, &cell("2"), "200", &day(3), &day(8));
    let c1 = put_member(&s, &cell("3"), "300", &day(10), &day(10));
    let at = |a: &str| -> Vec<(String, String)> {
        let ms = s.slice_members(&open(&h), a).unwrap();
        check_members(&s, &ms, a);
        arg_of(&ms, "k").into_iter().zip(ms.iter().map(|m| m.version.receipt_hash.clone())).collect()
    };
    let p = |k: &str, h: &str| (k.to_string(), h.to_string());
    assert_eq!(at(&day(1)), []);
    assert_eq!(at(&day(2)), [p("1", &a1)]);
    // Between its tx and its asof, the late-signed version is not an answer.
    assert_eq!(at(&day(3)), [p("1", &a1)]);
    assert_eq!(at("2024-01-07T23:59:59.999999Z"), [p("1", &a2)]);
    // The correction replaces the member's version from its asof on.
    assert_eq!(at(&day(6)), [p("1", &a2)]);
    assert_eq!(at("2024-01-05T23:59:59.999999Z"), [p("1", &a1)]);
    assert_eq!(at(&day(8)), [p("1", &a2), p("2", &b1)]);
    assert_eq!(at(&day(10)), [p("1", &a2), p("2", &b1), p("3", &c1)]);
    assert_eq!(at(END), [p("1", &a2), p("2", &b1), p("3", &c1)]);
}

#[test]
fn empty_slices_and_malformed_arguments() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    // No cells of the function at all.
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    assert!(s.slice_members(&open(&h), END).unwrap().is_empty());
    assert!(s.slice_members(&open(&"0".repeat(64)), END).unwrap().is_empty());
    put_member(&s, &slice_cell("f", &h, &[("k", sc(Type::Number, "1"))]), "1", &day(5), &day(5));
    // Before every version; a range holding none.
    assert!(s.slice_members(&open(&h), &day(4)).unwrap().is_empty());
    assert!(s.slice_members(&range(&h, "number", "2", "3"), END).unwrap().is_empty());
    assert_eq!(s.slice_members(&open(&h), &day(5)).unwrap().len(), 1);
    for bad in ["", "2024-01-01", "2024-01-01T00:00:00Z", "\0"] {
        assert_eq!(s.slice_members(&open(&h), bad).unwrap_err().code(), Some(ErrorCode::CanonInstant), "{bad:?}");
    }
}

#[test]
fn versions_between_is_a_version_slice() {
    let db = TestDb::migrated();
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let cell = slice_cell("f", &h, &[("k", sc(Type::Number, "1"))]);
    let id = cell.id();
    // Versions at days 2, 4 (late-signed: asof day 9), 6, 8.
    let v2 = put_member(&s, &cell, "2", &day(2), &day(2));
    let v4 = put_member(&s, &cell, "4", &day(4), &day(9));
    let v6 = put_member(&s, &cell, "6", &day(6), &day(6));
    let v8 = put_member(&s, &cell, "8", &day(8), &day(8));
    let vb = |f: &str, t: &str, a: &str| -> Vec<String> {
        let ms = s.versions_between(&id, f, t, a).unwrap();
        for m in &ms {
            let r = Receipt::from_json(&m.receipt).unwrap();
            assert_eq!((r.hash(), r.tx(), r.asof()), (m.version.receipt_hash.as_str(), m.version.tx.as_str(), m.version.asof.as_str()));
            assert_eq!(m.version.cell_id, id);
        }
        ms.into_iter().map(|m| m.version.receipt_hash).collect()
    };
    // Inclusive ends, ascending by tx.
    assert_eq!(vb(&day(2), &day(8), END), [&v2, &v4, &v6, &v8]);
    assert_eq!(vb(&day(4), &day(6), END), [&v4, &v6]);
    assert_eq!(vb(&day(3), &day(7), END), [&v4, &v6]);
    assert_eq!(vb(&day(8), &day(8), END), [&v8]);
    // Empty when from is after to.
    assert!(vb(&day(8), &day(2), END).is_empty());
    // Answering as of A: tx <= A and the receipt's asof <= A (the late-signed one from day 9).
    assert_eq!(vb(&day(1), &day(9), &day(8)), [&v2, &v6, &v8]);
    assert_eq!(vb(&day(1), &day(9), &day(9)), [&v2, &v4, &v6, &v8]);
    assert_eq!(vb(&day(1), &day(9), &day(5)), [&v2]);
    assert!(vb(&day(1), &day(9), &day(1)).is_empty());
    // An unknown cell; malformed arguments.
    assert!(s.versions_between(&"0".repeat(64), &day(1), END, END).unwrap().is_empty());
    let code = |e: datumesh_store::Error| e.code();
    assert_eq!(code(s.versions_between("x", &day(1), END, END).unwrap_err()), Some(ErrorCode::CanonHex));
    assert_eq!(code(s.versions_between(&id, "x", END, END).unwrap_err()), Some(ErrorCode::CanonInstant));
    assert_eq!(code(s.versions_between(&id, &day(1), "x", END).unwrap_err()), Some(ErrorCode::CanonInstant));
    assert_eq!(code(s.versions_between(&id, &day(1), END, "x").unwrap_err()), Some(ErrorCode::CanonInstant));
    // slice_members of the version slice is the same.
    let vs = slice(&format!(r#"{{"cell":{},"from":"{}","to":"{}"}}"#, String::from_utf8(cell.canonical()).unwrap(), day(3), day(9)));
    let ms = s.slice_members(&vs, &day(8)).unwrap();
    assert_eq!(ms.iter().map(|m| m.version.receipt_hash.clone()).collect::<Vec<_>>(), [&v6, &v8]);
}

/// 100,000 cells of one function (one argument `k`, a number 0..99999, and a version each):
/// the unbounded slice has all of them in numeric order; a range of 1,000 is a range scan of
/// the typed-argument index (checked with the server's statistics: no sequential scan of the
/// index tables), as is a bound argument.
#[test]
fn a_slice_over_100000_cells() {
    let db = TestDb::migrated();
    let s = std::sync::Arc::new(db.store_with(datumesh_store::PoolOptions::new().max_size(8)));
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number), ("tag", Type::String)], "big")).unwrap();
    const N: usize = 100_000;
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let (s, h) = (s.clone(), h.clone());
            std::thread::spawn(move || {
                for i in (t..N).step_by(8) {
                    let cell = slice_cell(
                        "f",
                        &h,
                        &[("k", sc(Type::Number, &i.to_string())), ("tag", sc(Type::String, if i % 2 == 0 { "even" } else { "odd" }))],
                    );
                    put_member(&s, &cell, &i.to_string(), &day(1), &day(1));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    db.client().batch_execute("ANALYZE").unwrap();
    let text = |args: &str| slice(&format!(r#"{{"args":{{{args}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#));

    let all = s.slice_members(&text(r#""k":"*","tag":"*""#), &day(2)).unwrap();
    assert_eq!(all.len(), N);
    let keys = arg_of(&all, "k");
    assert!(keys.iter().enumerate().all(|(i, k)| *k == i.to_string()), "not in numeric order");

    let seq_scans = || -> i64 {
        // The server's statistics reach the view once the store's backend is idle (PG 15+
        // flushes them within about a second).
        std::thread::sleep(std::time::Duration::from_millis(2500));
        db.client()
            .query_one(
                "SELECT coalesce(sum(seq_scan), 0)::bigint FROM pg_stat_user_tables WHERE relname IN ('cells', 'cell_args')",
                &[],
            )
            .unwrap()
            .get(0)
    };
    let before = seq_scans();
    let ms = s.slice_members(&text(r#""k":{"hi":"50999","lo":"50000","type":"number"},"tag":"*""#), &day(2)).unwrap();
    assert_eq!(arg_of(&ms, "k"), (50_000..51_000).map(|i| i.to_string()).collect::<Vec<_>>());
    let ms = s.slice_members(&text(r#""k":{"type":"number","value":"77777"},"tag":"*""#), &day(2)).unwrap();
    assert_eq!(arg_of(&ms, "k"), ["77777"]);
    let ms = s.slice_members(&text(r#""k":{"hi":"60","lo":"41","type":"number"},"tag":{"type":"string","value":"odd"}"#), &day(2)).unwrap();
    assert_eq!(arg_of(&ms, "k"), (41..60).step_by(2).map(|i| i.to_string()).collect::<Vec<_>>());
    assert_eq!(seq_scans(), before, "a bounded slice scanned a table sequentially");
}
