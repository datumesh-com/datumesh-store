//! Store 0.6.4, CONTRACT-0.5.1 S1 and S3: the stored sort key of a cell, its migration, and
//! the class-bindings index. Both backends (migrations PostgreSQL 15, SQLite 11).
//!
//! ASSUMED API (additive; the contract leaves the exact shape open):
//!
//! ```text
//! // datumesh_store::SliceValue (already #[non_exhaustive]) gains a public field:
//! pub sort_key: Vec<u8>,   // the member's cells.sort_key
//! // on Store (default body: correct but slow, built on slice_values, or Unsupported):
//! fn slice_page(&self, slice: &Slice, asof: &str, after: Option<&[u8]>, limit: usize)
//!     -> Result<Vec<SliceValue>>;
//! // after = Some(key): members whose sort key is STRICTLY greater; None: from the start.
//! ```
//!
//! Schema assumed: `cells.sort_key` is `bytea` (PostgreSQL) / `BLOB` (SQLite), NOT NULL in
//! effect (a cell with no arguments has the empty key), and the byte order of the key is the
//! order of SPEC 5.6 / 15.13: arguments in ascending name order, each in its type's order
//! (numbers by value, dates, instants and strings by lexical form in byte order = C collation,
//! false before true), joined so that a shorter prefix sorts first. A fixed argument of a
//! slice puts the same bytes in every member's key. The index is `(fn_hash, ns, fn, sort_key)`
//! on `cells`; S3 is `class_bindings (class_hash, class_name, tx)`.
//!
//! Numbers are as SPEC 3.1: exact decimals with no exponent form, so "large and small
//! exponents" are long digit strings (1 followed by 100 zeros) and tiny decimals (up to 127
//! characters, the 128 limit with sign).
//!
//! The order is observed through `slice_page(slice, asof, None, big)`: the members' values,
//! and their `sort_key`s, which must be strictly ascending, and equal to the stored column.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Scalar, Type};
use datumesh_store::{migrate, CellWrite, SliceValue, SqliteOptions};

const T: &str = "2024-01-01T00:00:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";
const BIG: usize = 1_000_000;

/// `{"args": {name: spec, ...}, "fn": "f", ...}` with each spec the JSON text of the argument
/// (`"*"`, a fixed `{"type":..,"value":..}` or a range).
fn slice_of(h: &str, f: &str, args: &[(&str, &str)]) -> datumesh_core::Slice {
    let a: Vec<String> = args.iter().map(|(n, s)| format!(r#""{n}":{s}"#)).collect();
    slice(&format!(r#"{{"args":{{{}}},"fn":"{f}","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#, a.join(",")))
}

/// Stores the cells (each a list of arguments), the cell `i` with the value `values[i]`.
fn put_all(s: &TStore, f: &str, h: &str, cells: &[Vec<(&str, Scalar)>], values: &[String]) {
    for chunk in (0..cells.len()).collect::<Vec<_>>().chunks(500) {
        let built: Vec<(String, Vec<u8>)> = chunk
            .iter()
            .map(|&i| {
                let c = slice_cell(f, h, &cells[i]);
                (c.id(), slice_receipt(&c, &values[i], T, T))
            })
            .collect();
        let items: Vec<CellWrite> = built.iter().map(|(id, r)| CellWrite::new(r, Some((id.as_str(), T)))).collect();
        s.put_cells(&items).unwrap();
    }
}

fn values_of(vs: &[SliceValue]) -> Vec<String> {
    vs.iter().map(|v| v.value.lexical().to_string()).collect()
}

fn keys_ascend(vs: &[SliceValue]) {
    for w in vs.windows(2) {
        assert!(w[0].sort_key < w[1].sort_key, "{:?} !< {:?}", w[0].sort_key, w[1].sort_key);
    }
}

/// A deterministic shuffle: index order i * 7 mod n (n coprime to 7), so insertion order is
/// neither key order nor its reverse.
fn shuffled<T: Clone>(v: &[T]) -> Vec<T> {
    let n = v.len();
    assert!(n % 7 != 0);
    (0..n).map(|i| v[(i * 7) % n].clone()).collect()
}

/// One-argument function `f(k: <t>)` with the cells `ks` (lexical forms, given in any order);
/// the page of the open slice lists the cells in order `want`.
fn single_arg_order(b: Backend, t: Type, ks: Vec<String>, want: Vec<String>) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", t)], "f")).unwrap();
    let cells: Vec<Vec<(&str, Scalar)>> = ks.iter().map(|k| vec![("k", sc(t, k))]).collect();
    // The value of a cell is its position in `ks`, so the order read back names the keys.
    let values: Vec<String> = (0..ks.len()).map(|i| i.to_string()).collect();
    put_all(&s, "f", &h, &cells, &values);
    let vs = s.slice_page(&slice_of(&h, "f", &[("k", r#""*""#)]), END, None, BIG).unwrap();
    let got: Vec<String> = values_of(&vs).iter().map(|i| ks[i.parse::<usize>().unwrap()].clone()).collect();
    assert_eq!(got, want);
    keys_ascend(&vs);
    assert_eq!(vs.len(), ks.len());
}

fn numbers_are_ordered_by_value_not_by_text(b: Backend) {
    let big = format!("1{}", "0".repeat(100));
    let tiny = format!("0.{}1", "0".repeat(124)); // 127 characters
    let want: Vec<String> = [
        format!("-{big}"),
        "-123456789.12345679".into(),
        "-123456789.123456789".into(),
        "-100".into(),
        "-10.5".into(),
        "-10".into(),
        "-9".into(),
        "-2".into(),
        "-1.5".into(),
        "-1".into(),
        "-0.5".into(),
        "-0.001".into(),
        format!("-{tiny}"),
        "0".into(),
        tiny.clone(),
        "0.001".into(),
        "0.5".into(),
        "1".into(),
        "1.0000000000000000000000000000001".into(),
        "1.5".into(),
        "2".into(),
        "9".into(),
        "10".into(),
        "10.5".into(),
        "99".into(),
        "100".into(),
        "123456789.123456789".into(),
        "123456789.12345679".into(),
        big.clone(),
    ]
    .into_iter()
    .collect();
    // The list is in numeric order by construction; insert in lexical order of the text, which
    // is wrong for most neighbours, then in a shuffled order.
    let mut lexical = want.clone();
    lexical.sort();
    assert_ne!(lexical, want);
    single_arg_order(b, Type::Number, lexical.clone(), want.clone());
    single_arg_order(b, Type::Number, shuffled(&want), want);
}

fn dates_are_ordered_chronologically(b: Backend) {
    let want: Vec<String> = ["0001-01-01", "1999-12-31", "2000-01-01", "2024-02-29", "2024-03-01", "2024-12-31", "9999-12-31"]
        .map(String::from)
        .to_vec();
    single_arg_order(b, Type::Date, shuffled(&want), want.clone());
    let mut rev = want.clone();
    rev.reverse();
    single_arg_order(b, Type::Date, rev, want);
}

fn instants_are_ordered_chronologically(b: Backend) {
    let want: Vec<String> = [
        "0001-01-01T00:00:00.000000Z",
        "2024-03-15T15:59:59.999999Z",
        "2024-03-15T16:00:00.000000Z",
        "2024-03-15T16:00:00.000001Z",
        "9999-12-31T23:59:59.999999Z",
    ]
    .map(String::from)
    .to_vec();
    single_arg_order(b, Type::Instant, shuffled(&want), want);
}

fn strings_are_ordered_by_bytes_under_c_collation(b: Backend) {
    // Byte order, which is not any locale's: capitals before lower case, a space before both,
    // a prefix before its extensions, and non-ASCII (UTF-8 lead bytes >= 0xC3) after ASCII.
    let mut want: Vec<String> =
        ["", " ", "-", "0", "A", "B", "Z", "_", "a", "a b", "aa", "ab", "abc", "b", "z", "é", "Ünï", "日本", "\u{1F600}"]
            .map(String::from)
            .to_vec();
    want.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    assert_eq!(want.first().map(String::as_str), Some(""));
    assert!(want.iter().position(|x| x == "Z") < want.iter().position(|x| x == "a"));
    assert!(want.iter().position(|x| x == "z") < want.iter().position(|x| x == "é"));
    single_arg_order(b, Type::String, shuffled(&want), want);
}

fn booleans_put_false_before_true(b: Backend) {
    single_arg_order(b, Type::Boolean, vec!["true".into(), "false".into()], vec!["false".into(), "true".into()]);
}

/// Hundredths as a SPEC 3.1 number.
fn hundredths(h: i64) -> String {
    let (sign, a) = (if h < 0 { "-" } else { "" }, h.unsigned_abs());
    let (i, f) = (a / 100, a % 100);
    match f {
        0 => format!("{sign}{i}"),
        f if f % 10 == 0 => format!("{sign}{i}.{}", f / 10),
        f => format!("{sign}{i}.{f:02}"),
    }
}

fn mixed_multi_argument_keys_order_by_name_then_by_type(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    // Argument names a < b < c < d; declared in another order.
    let h = s
        .put_descriptor(&stored_descriptor(
            &[("d", Type::Date), ("b", Type::String), ("c", Type::Boolean), ("a", Type::Number)],
            "f",
        ))
        .unwrap();
    let mut tuples: Vec<(i64, String, bool, String)> = vec![];
    for a in [-150, -5, 0, 5, 100, 1000] {
        for bb in ["a", "ab", "b"] {
            for c in [false, true] {
                for d in ["2023-12-31", "2024-01-01"] {
                    tuples.push((a, bb.to_string(), c, d.to_string()));
                }
            }
        }
    }
    let tuples = shuffled(&tuples);
    let cells: Vec<Vec<(&str, Scalar)>> = tuples
        .iter()
        .map(|(a, bb, c, d)| {
            vec![
                ("a", sc(Type::Number, &hundredths(*a))),
                ("b", sc(Type::String, bb)),
                ("c", sc(Type::Boolean, if *c { "true" } else { "false" })),
                ("d", sc(Type::Date, d)),
            ]
        })
        .collect();
    let values: Vec<String> = (0..tuples.len()).map(|i| i.to_string()).collect();
    put_all(&s, "f", &h, &cells, &values);
    let mut order: Vec<usize> = (0..tuples.len()).collect();
    order.sort_by(|&x, &y| tuples[x].cmp(&tuples[y]));
    let sl = slice_of(&h, "f", &[("a", r#""*""#), ("b", r#""*""#), ("c", r#""*""#), ("d", r#""*""#)]);
    let vs = s.slice_page(&sl, END, None, BIG).unwrap();
    let got: Vec<usize> = values_of(&vs).iter().map(|v| v.parse().unwrap()).collect();
    assert_eq!(got, order);
    keys_ascend(&vs);
    // Fixing arguments keeps the order of the open ones (and the members' keys are the same).
    let fixed = slice_of(&h, "f", &[("a", r#""*""#), ("b", r#"{"type":"string","value":"ab"}"#), ("c", r#""*""#), ("d", r#"{"type":"date","value":"2024-01-01"}"#)]);
    let fv = s.slice_page(&fixed, END, None, BIG).unwrap();
    let want: Vec<usize> = order.iter().copied().filter(|&i| tuples[i].1 == "ab" && tuples[i].3 == "2024-01-01").collect();
    assert_eq!(want.len(), 12);
    assert_eq!(values_of(&fv).iter().map(|v| v.parse::<usize>().unwrap()).collect::<Vec<_>>(), want);
    for m in &fv {
        let same = vs.iter().find(|x| x.version.cell_id == m.version.cell_id).unwrap();
        assert_eq!(m.sort_key, same.sort_key, "a cell has one sort key, whatever the slice");
    }
}

fn the_name_order_not_the_declaration_order_makes_the_key(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    // `alpha` sorts before `zeta`: the key is (alpha, zeta) whatever the order written.
    let h = s.put_descriptor(&stored_descriptor(&[("zeta", Type::Number), ("alpha", Type::String)], "f")).unwrap();
    let rows = [("b", "1"), ("a", "2"), ("a", "10"), ("b", "-1")];
    let cells: Vec<Vec<(&str, Scalar)>> =
        rows.iter().map(|(a, z)| vec![("zeta", sc(Type::Number, z)), ("alpha", sc(Type::String, a))]).collect();
    let values: Vec<String> = (0..rows.len()).map(|i| i.to_string()).collect();
    put_all(&s, "f", &h, &cells, &values);
    let sl = slice_of(&h, "f", &[("zeta", r#""*""#), ("alpha", r#""*""#)]);
    let got: Vec<usize> = values_of(&s.slice_page(&sl, END, None, BIG).unwrap()).iter().map(|v| v.parse().unwrap()).collect();
    // (a,2) (a,10) (b,-1) (b,1)
    assert_eq!(got, [1, 2, 3, 0]);
}

fn a_shorter_prefix_sorts_first_whatever_follows(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("s1", Type::String), ("s2", Type::String)], "f")).unwrap();
    // Plain concatenation would put ("a","z") = "az" after ("ab","a") = "aba", and ("a","b")
    // = "ab" level with ("", "ab"); the join must not.
    let mut rows: Vec<(String, String)> = vec![];
    for x in ["", "a", "ab", "abc", "b"] {
        for y in ["", "a", "ab", "z", "!"] {
            rows.push((x.to_string(), y.to_string()));
        }
    }
    let rows = shuffled(&rows);
    let cells: Vec<Vec<(&str, Scalar)>> =
        rows.iter().map(|(x, y)| vec![("s1", sc(Type::String, x)), ("s2", sc(Type::String, y))]).collect();
    let values: Vec<String> = (0..rows.len()).map(|i| i.to_string()).collect();
    put_all(&s, "f", &h, &cells, &values);
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&p, &q| (rows[p].0.as_bytes(), rows[p].1.as_bytes()).cmp(&(rows[q].0.as_bytes(), rows[q].1.as_bytes())));
    let sl = slice_of(&h, "f", &[("s1", r#""*""#), ("s2", r#""*""#)]);
    let vs = s.slice_page(&sl, END, None, BIG).unwrap();
    let got: Vec<usize> = values_of(&vs).iter().map(|v| v.parse().unwrap()).collect();
    assert_eq!(got, order);
    keys_ascend(&vs);
    // The same for numbers followed by a string: (2, "a") < (10, "") although "2" > "10" as text.
    let h2 = s.put_descriptor(&stored_descriptor(&[("n", Type::Number), ("t", Type::String)], "g")).unwrap();
    let rows2 = [("10", ""), ("2", "zz"), ("2", "a"), ("1", "q"), ("100", "a")];
    let cells2: Vec<Vec<(&str, Scalar)>> =
        rows2.iter().map(|(n, t)| vec![("n", sc(Type::Number, n)), ("t", sc(Type::String, t))]).collect();
    let values2: Vec<String> = (0..rows2.len()).map(|i| i.to_string()).collect();
    put_all(&s, "g", &h2, &cells2, &values2);
    let sl2 = slice_of(&h2, "g", &[("n", r#""*""#), ("t", r#""*""#)]);
    let got2: Vec<usize> = values_of(&s.slice_page(&sl2, END, None, BIG).unwrap()).iter().map(|v| v.parse().unwrap()).collect();
    assert_eq!(got2, [3, 2, 1, 0, 4]);
}

fn the_key_is_a_column_of_cells_and_slice_page_returns_it(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let ks = ["3", "-4.5", "20"];
    let cells: Vec<Vec<(&str, Scalar)>> = ks.iter().map(|k| vec![("k", sc(Type::Number, k))]).collect();
    put_all(&s, "f", &h, &cells, &ks.map(String::from));
    assert_eq!(db.int("SELECT count(*) FROM cells WHERE sort_key IS NULL"), 0);
    let vs = s.slice_page(&slice_of(&h, "f", &[("k", r#""*""#)]), END, None, BIG).unwrap();
    assert_eq!(values_of(&vs), ["-4.5", "3", "20"]);
    for v in &vs {
        assert!(!v.sort_key.is_empty());
        let hex: String = v.sort_key.iter().map(|x| format!("{x:02x}")).collect();
        let stored = db.texts(&format!(
            "SELECT {} FROM cells WHERE cell_id = '{}'",
            db.pick("encode(sort_key, 'hex')", "hex(sort_key)"),
            v.version.cell_id
        ));
        assert_eq!(stored[0][0].to_lowercase(), hex, "the page's key is the column's");
    }
    // The key is a function of the cell's arguments alone: the same cell again (a new version)
    // keeps it.
    let c = slice_cell("f", &h, &[("k", sc(Type::Number, "3"))]);
    put_member(&s, &c, "4", "2024-02-01T00:00:00.000000Z", "2024-02-01T00:00:00.000000Z");
    let again = s.slice_page(&slice_of(&h, "f", &[("k", r#""*""#)]), END, None, BIG).unwrap();
    assert_eq!(again.iter().map(|v| v.sort_key.clone()).collect::<Vec<_>>(), vs.iter().map(|v| v.sort_key.clone()).collect::<Vec<_>>());
    assert_eq!(db.count("cells"), 3);
}

fn a_cell_without_arguments_has_the_empty_key(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[], "f")).unwrap();
    let c = slice_cell("f", &h, &[]);
    put_member(&s, &c, "1", T, T);
    assert_eq!(db.int("SELECT count(*) FROM cells WHERE sort_key IS NULL"), 0);
    assert_eq!(db.int("SELECT count(*) FROM cells"), 1);
}

backends!(
    numbers_are_ordered_by_value_not_by_text,
    dates_are_ordered_chronologically,
    instants_are_ordered_chronologically,
    strings_are_ordered_by_bytes_under_c_collation,
    booleans_put_false_before_true,
    mixed_multi_argument_keys_order_by_name_then_by_type,
    the_name_order_not_the_declaration_order_makes_the_key,
    a_shorter_prefix_sorts_first_whatever_follows,
    the_key_is_a_column_of_cells_and_slice_page_returns_it,
    a_cell_without_arguments_has_the_empty_key,
    the_migration_fills_sort_keys_on_existing_rows,
    migration_15_and_11_are_the_last_and_make_the_indexes,
);

// ---- Migration: apply 1..=14, store, then 15 (SQLite 1..=10, then 11) -------------------------

fn migrate_to(db: &TestDb, b: Backend, up_to: usize) {
    let old: Vec<&migrate::Migration> = match b {
        Backend::Pg => migrate::MIGRATIONS[..up_to].iter().collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS[..up_to].iter().collect(),
    };
    db.exec(db.pick(
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))) STRICT",
    ))
    .unwrap();
    for m in &old {
        db.exec(m.sql).unwrap();
        db.exec(&format!(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ({}, '{}', '{}')",
            m.version,
            m.name,
            m.checksum()
        ))
        .unwrap();
    }
}

fn apply_rest(db: &TestDb, b: Backend) -> Vec<u32> {
    match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    }
}

fn the_migration_fills_sort_keys_on_existing_rows(b: Backend) {
    let (old, last) = if b == Backend::Pg { (14, 15) } else { (10, 11) };
    let db = TestDb::empty(b);
    migrate_to(&db, b, old);
    assert_eq!(
        db.int(db.pick(
            "SELECT count(*) FROM information_schema.columns WHERE table_name = 'cells' AND column_name = 'sort_key' AND table_schema = current_schema()",
            "SELECT count(*) FROM pragma_table_info('cells') WHERE name = 'sort_key'",
        )),
        0,
        "no sort key before the migration"
    );
    // Cells as 0.6.3 wrote them: two functions, several versions, a mix of argument types.
    let s = db.bare_store();
    s.put_namespace(SLICE_HOST, &slice_keydoc(), VALIDATED_AT).unwrap();
    let h1 = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let h2 = s.put_descriptor(&stored_descriptor(&[("n", Type::Number), ("t", Type::String)], "g")).unwrap();
    let ks = ["100", "-7.5", "2", "0", "-80", "13.25"];
    for k in ks {
        put_member(&s, &slice_cell("f", &h1, &[("k", sc(Type::Number, k))]), k, T, T);
    }
    put_member(&s, &slice_cell("f", &h1, &[("k", sc(Type::Number, "2"))]), "2.5", "2024-02-01T00:00:00.000000Z", "2024-02-01T00:00:00.000000Z");
    let gs = [("10", "a"), ("2", "b"), ("2", "a"), ("-1", "z")];
    for (n, t) in gs {
        put_member(&s, &slice_cell("g", &h2, &[("n", sc(Type::Number, n)), ("t", sc(Type::String, t))]), "1", T, T);
    }
    assert_eq!(db.count("cells"), 10);
    drop(s);

    assert_eq!(apply_rest(&db, b), [last], "only the sort-key migration is applied");
    assert_eq!(db.int("SELECT count(*) FROM cells WHERE sort_key IS NULL"), 0, "every existing row is filled");
    assert_eq!(db.count("cells"), 10);

    let s = db.store();
    let vs = s.slice_page(&slice_of(&h1, "f", &[("k", r#""*""#)]), END, None, BIG).unwrap();
    assert_eq!(values_of(&vs), ["-80", "-7.5", "0", "2.5", "13.25", "100"]);
    keys_ascend(&vs);
    let gv = s.slice_page(&slice_of(&h2, "g", &[("n", r#""*""#), ("t", r#""*""#)]), END, None, BIG).unwrap();
    assert_eq!(gv.len(), 4);
    keys_ascend(&gv);
    // (-1,z) (2,a) (2,b) (10,a): the cells' own order, not insertion order.
    let ids: Vec<String> = [("-1", "z"), ("2", "a"), ("2", "b"), ("10", "a")]
        .iter()
        .map(|(n, t)| slice_cell("g", &h2, &[("n", sc(Type::Number, n)), ("t", sc(Type::String, t))]).id())
        .collect();
    assert_eq!(gv.iter().map(|v| v.version.cell_id.clone()).collect::<Vec<_>>(), ids);

    // A cell stored after the migration gets its key from the trigger, in its place.
    put_member(&s, &slice_cell("f", &h1, &[("k", sc(Type::Number, "-7.25"))]), "-7.25", T, T);
    assert_eq!(db.int("SELECT count(*) FROM cells WHERE sort_key IS NULL"), 0);
    let vs = s.slice_page(&slice_of(&h1, "f", &[("k", r#""*""#)]), END, None, BIG).unwrap();
    assert_eq!(values_of(&vs), ["-80", "-7.5", "-7.25", "0", "2.5", "13.25", "100"]);
    keys_ascend(&vs);
}

fn migration_15_and_11_are_the_last_and_make_the_indexes(b: Backend) {
    let n = match b {
        Backend::Pg => migrate::MIGRATIONS.len(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.len(),
    };
    assert_eq!(n, if b == Backend::Pg { 15 } else { 11 }, "0.6.4 adds one migration: PostgreSQL 15, SQLite 11");
    let db = TestDb::migrated(b);
    let squash = |s: String| s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    // S1: the index (fn_hash, ns, fn, sort_key) on cells.
    let cells_ix = db.texts(db.pick(
        "SELECT indexdef FROM pg_indexes WHERE tablename = 'cells' AND schemaname = current_schema()",
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'cells' AND sql IS NOT NULL",
    ));
    assert!(
        cells_ix.iter().any(|r| squash(r[0].clone()).contains("(fn_hash,ns,fn,sort_key)")),
        "an index on cells (fn_hash, ns, fn, sort_key): {cells_ix:?}"
    );
    // S3: the index (class_hash, class_name, tx) on class_bindings.
    let cb_ix = db.texts(db.pick(
        "SELECT indexdef FROM pg_indexes WHERE tablename = 'class_bindings' AND schemaname = current_schema()",
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'class_bindings' AND sql IS NOT NULL",
    ));
    assert!(
        cb_ix.iter().any(|r| squash(r[0].clone()).contains("(class_hash,class_name,tx)")),
        "an index on class_bindings (class_hash, class_name, tx): {cb_ix:?}"
    );
}

// ---- S3 on PostgreSQL: a lookup of bindings by class hash uses the index ---------------------

#[test]
fn pg_class_bindings_by_hash_use_the_index() {
    let db = PgDb::migrated();
    let mut c = db.client();
    c.batch_execute("SET enable_seqscan = off").unwrap();
    let rows = c
        .query(
            "EXPLAIN SELECT class_name, tx FROM class_bindings WHERE class_hash = 'ab' ORDER BY class_name, tx",
            &[],
        )
        .unwrap();
    let plan: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
    let text = plan.join("\n");
    assert!(text.contains("Index"), "{text}");
    assert!(!text.contains("Seq Scan"), "{text}");
}
