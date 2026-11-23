//! Store 0.6.4: the public helper `datumesh_store::sort_key`, which http uses to turn the
//! SPEC 5.6 `after` key into a `slice_page` cursor. Both backends.
//!
//! ASSUMED API (default features, not sqlite-only):
//!
//! ```text
//! pub fn sort_key(args: &[(&str /*name*/, &str /*type*/, &str /*lexical*/)]) -> Vec<u8>
//! ```
//!
//! `args` are in ascending name order; the type is `number`, `date`, `instant`, `string` or
//! `boolean`. The bytes are exactly those stored in `cells.sort_key` and returned in
//! `SliceValue.sort_key`, on PostgreSQL and on SQLite.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Scalar, Type};
use datumesh_store::{sort_key, CellWrite};

const T: &str = "2024-01-01T00:00:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";

type Arg = (&'static str, Type, String);

fn t_name(t: Type) -> &'static str {
    t.as_str()
}

/// Stores `rows` as cells of `f` (value = row index), reads them back through an open slice
/// and compares each member's key with the helper's and with the column's.
fn check(b: Backend, decls: &[(&str, Type)], rows: Vec<Vec<Arg>>) -> usize {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(decls, "f")).unwrap();
    let cells: Vec<datumesh_core::Cell> = rows
        .iter()
        .map(|r| {
            let a: Vec<(&str, Scalar)> = r.iter().map(|(n, t, l)| (*n, sc(*t, l))).collect();
            slice_cell("f", &h, &a)
        })
        .collect();
    for chunk in (0..rows.len()).collect::<Vec<_>>().chunks(100) {
        let built: Vec<(String, Vec<u8>)> =
            chunk.iter().map(|&i| (cells[i].id(), slice_receipt(&cells[i], &i.to_string(), T, T))).collect();
        let items: Vec<CellWrite> = built.iter().map(|(id, r)| CellWrite::new(r, Some((id.as_str(), T)))).collect();
        s.put_cells(&items).unwrap();
    }
    let args: Vec<String> = decls.iter().map(|(n, _)| format!(r#""{n}":"*""#)).collect();
    let sl = slice(&format!(r#"{{"args":{{{}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#, args.join(",")));
    let page = s.slice_page(&sl, END, None, 1_000_000).unwrap();
    assert_eq!(page.len(), rows.len());
    for m in &page {
        let i: usize = m.value.lexical().parse().unwrap();
        let a: Vec<(&str, &str, &str)> = rows[i].iter().map(|(n, t, l)| (*n, t_name(*t), l.as_str())).collect();
        let mine = sort_key(&a);
        assert_eq!(mine, m.sort_key, "helper vs returned key for {a:?}");
        let hex: String = mine.iter().map(|x| format!("{x:02x}")).collect();
        let col = db.texts(&format!(
            "SELECT {} FROM cells WHERE cell_id = '{}'",
            db.pick("encode(sort_key, 'hex')", "hex(sort_key)"),
            m.version.cell_id
        ));
        assert_eq!(col[0][0].to_lowercase(), hex, "helper vs stored column for {a:?}");
    }
    rows.len()
}

fn one(name: &'static str, t: Type, vals: &[String]) -> Vec<Vec<Arg>> {
    vals.iter().map(|v| vec![(name, t, v.clone())]).collect()
}

fn single_argument_cells_have_the_helpers_key(b: Backend) {
    let mut nums: Vec<String> = ["0", "1", "-1", "0.5", "-0.5", "2", "-2", "9", "10", "-10", "10.5", "99", "100", "-100", "0.001", "-0.001"]
        .map(String::from)
        .to_vec();
    nums.push(format!("1{}", "0".repeat(100)));
    nums.push(format!("-1{}", "0".repeat(100)));
    nums.push(format!("0.{}1", "0".repeat(124)));
    nums.push(format!("-0.{}1", "0".repeat(124)));
    nums.push(format!("{}.{}", "9".repeat(60), "1".repeat(60)));
    nums.push(format!("-{}.{}", "9".repeat(60), "1".repeat(60)));
    for i in 0..40i64 {
        let h = (i - 20) * 137;
        let (sign, a) = (if h < 0 { "-" } else { "" }, h.unsigned_abs());
        let v = match (a / 100, a % 100) {
            (i, 0) => format!("{sign}{i}"),
            (i, f) if f % 10 == 0 => format!("{sign}{i}.{}", f / 10),
            (i, f) => format!("{sign}{i}.{f:02}"),
        };
        if !nums.contains(&v) {
            nums.push(v);
        }
    }
    let n = check(b, &[("k", Type::Number)], one("k", Type::Number, &nums));
    assert!(n > 50);
    let dates: Vec<String> = ["0001-01-01", "1999-12-31", "2000-01-01", "2024-02-29", "2024-03-01", "9999-12-31"].map(String::from).to_vec();
    check(b, &[("k", Type::Date)], one("k", Type::Date, &dates));
    let instants: Vec<String> = ["0001-01-01T00:00:00.000000Z", "2024-03-15T16:00:00.000001Z", "9999-12-31T23:59:59.999999Z"]
        .map(String::from)
        .to_vec();
    check(b, &[("k", Type::Instant)], one("k", Type::Instant, &instants));
    let strings: Vec<String> =
        ["", " ", "A", "Z", "a", "a b", "aa", "ab", "abc", "b", "é", "Ünï", "日本", "\u{1F600}", "x\"y\\z"].map(String::from).to_vec();
    check(b, &[("k", Type::String)], one("k", Type::String, &strings));
    check(b, &[("k", Type::Boolean)], one("k", Type::Boolean, &["false".into(), "true".into()]));
}

fn multi_argument_cells_have_the_helpers_key(b: Backend) {
    let mut rows: Vec<Vec<Arg>> = vec![];
    for a in ["-150.5", "-5", "0", "0.25", "100", "12345678901234567890.5"] {
        for s in ["", "a", "ab", "b", "é"] {
            for c in ["false", "true"] {
                for d in ["2023-12-31", "2024-01-01"] {
                    rows.push(vec![
                        ("a", Type::Number, a.into()),
                        ("b", Type::String, s.into()),
                        ("c", Type::Boolean, c.into()),
                        ("d", Type::Date, d.into()),
                    ]);
                }
            }
        }
    }
    assert_eq!(rows.len(), 120);
    check(b, &[("d", Type::Date), ("b", Type::String), ("c", Type::Boolean), ("a", Type::Number)], rows);
    // Strings that are prefixes of each other, then a number.
    let mut rows2: Vec<Vec<Arg>> = vec![];
    for x in ["", "a", "ab", "abc"] {
        for n in ["-3", "0", "2", "10"] {
            rows2.push(vec![("s", Type::String, x.into()), ("t", Type::Number, n.into())]);
        }
    }
    check(b, &[("s", Type::String), ("t", Type::Number)], rows2);
}

fn a_cell_without_arguments_has_the_empty_key_helper(b: Backend) {
    assert_eq!(sort_key(&[]), Vec::<u8>::new());
    let _ = b;
}

backends!(
    single_argument_cells_have_the_helpers_key,
    multi_argument_cells_have_the_helpers_key,
    a_cell_without_arguments_has_the_empty_key_helper,
);
