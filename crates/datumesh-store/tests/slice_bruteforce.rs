//! CONTRACT-perf S-T1: slices over a function with three arguments equal a brute-force
//! filter, computed in Rust over every cell written, for `slice_members`, `slice_values`,
//! `slice_changed_after` and `computed_cells`. The slice statements are rewritten in S5 with
//! identical results, so these are a safety net that already passes on 0.4.2.

#[macro_use]
mod common;

use common::*;
use datumesh_core::Type;

fn day(d: u32) -> String {
    format!("2024-01-{d:02}T00:00:00.000000Z")
}

/// One stored version of a cell.
#[derive(Clone)]
struct V {
    tx: String,
    asof: String,
    hash: String,
    value: String,
}

struct C {
    a: String,
    b: i64,
    c: i64,
    id: String,
    versions: Vec<V>,
    computed: Option<V>,
}

/// A constraint of the slice and of the brute-force filter.
enum Con<T> {
    Val(T),
    Range(T, T),
    Open,
}

impl<T: PartialOrd + Clone> Con<T> {
    fn ok(&self, x: &T) -> bool {
        match self {
            Con::Val(v) => v == x,
            Con::Range(lo, hi) => lo <= x && x <= hi,
            Con::Open => true,
        }
    }
    fn is_val(&self) -> bool {
        matches!(self, Con::Val(_))
    }
}

struct Spec {
    a: Con<String>,
    b: Con<i64>,
    c: Con<i64>,
}

impl Spec {
    fn matches(&self, c: &C) -> bool {
        self.a.ok(&c.a) && self.b.ok(&c.b) && self.c.ok(&c.c)
    }
    /// Key order: the open and ranged arguments in ascending name order, each in its type's order.
    fn cmp(&self, x: &C, y: &C) -> std::cmp::Ordering {
        let mut o = std::cmp::Ordering::Equal;
        if !self.a.is_val() {
            o = o.then(x.a.cmp(&y.a));
        }
        if !self.b.is_val() {
            o = o.then(x.b.cmp(&y.b));
        }
        if !self.c.is_val() {
            o = o.then(x.c.cmp(&y.c));
        }
        o
    }
    fn json(&self, h: &str) -> String {
        fn arg<T: ToString>(c: &Con<T>, t: &str) -> String {
            match c {
                Con::Val(v) => format!(r#"{{"type":"{t}","value":"{}"}}"#, v.to_string()),
                Con::Range(lo, hi) => {
                    format!(r#"{{"hi":"{}","lo":"{}","type":"{t}"}}"#, hi.to_string(), lo.to_string())
                }
                Con::Open => r#""*""#.to_string(),
            }
        }
        format!(
            r#"{{"args":{{"a":{},"b":{},"c":{}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#,
            arg(&self.a, "string"),
            arg(&self.b, "number"),
            arg(&self.c, "number")
        )
    }
}

/// The version of `c` answering as of `asof` (`tx <= asof`, receipt asof `<= asof`, latest tx).
fn answering<'a>(vs: &'a [V], asof: &str) -> Option<&'a V> {
    vs.iter().filter(|v| v.tx.as_str() <= asof && v.asof.as_str() <= asof).max_by(|x, y| x.tx.cmp(&y.tx))
}

fn world(s: &TStore) -> (String, Vec<C>) {
    let h = s
        .put_descriptor(&stored_descriptor(&[("a", Type::String), ("b", Type::Number), ("c", Type::Number)], "f"))
        .unwrap();
    let mut cells = Vec::new();
    let mut n = 0;
    for ai in 0..6 {
        for b in 1..=4i64 {
            for c in 1..=5i64 {
                n += 1;
                let a = format!("e{ai}");
                let cell = slice_cell(
                    "f",
                    &h,
                    &[
                        ("a", sc(Type::String, &a)),
                        ("b", sc(Type::Number, &b.to_string())),
                        ("c", sc(Type::Number, &c.to_string())),
                    ],
                );
                let mut versions = Vec::new();
                let mut put = |s: &TStore, value: String, tx: String, asof: String| {
                    let hash = put_member(s, &cell, &value, &tx, &asof);
                    versions.push(V { tx, asof, hash, value });
                };
                put(s, format!("{n}"), day(1), day(1));
                if n % 7 == 0 {
                    // A later version, answering from day 3.
                    put(s, format!("{}", n + 1000), day(3), day(3));
                } else if n % 11 == 0 {
                    // Signed late: stored at day 2, answering only from day 5.
                    put(s, format!("{}", n + 2000), day(2), day(5));
                }
                let computed = (n % 5 == 0).then(|| {
                    let (tx, asof) = (day(2), day(2));
                    let value = format!("{}", n + 3000);
                    let hash = s.put_receipt(&slice_receipt(&cell, &value, &tx, &asof)).unwrap();
                    s.put_computed(&cell.id(), &tx, &hash).unwrap();
                    V { tx, asof, hash, value }
                });
                cells.push(C { a, b, c, id: cell.id(), versions, computed });
            }
        }
    }
    (h, cells)
}

fn lex(cell: &datumesh_core::Cell, name: &str) -> String {
    cell.args[name].scalar().unwrap().lexical().to_string()
}

fn check(s: &TStore, h: &str, cells: &[C], spec: &Spec, expect_nonempty: bool) {
    let sl = slice(&spec.json(h));
    let mut any = false;
    for asof in [day(1), day(2), day(4), day(6)] {
        let mut want: Vec<(&C, &V)> = cells
            .iter()
            .filter(|c| spec.matches(c))
            .filter_map(|c| answering(&c.versions, &asof).map(|v| (c, v)))
            .collect();
        want.sort_by(|x, y| spec.cmp(x.0, y.0));
        any |= !want.is_empty();

        let ms = s.slice_members(&sl, &asof).unwrap();
        assert_eq!(
            ms.iter()
                .map(|m| (m.version.cell_id.clone(), m.version.tx.clone(), m.version.receipt_hash.clone()))
                .collect::<Vec<_>>(),
            want.iter().map(|(c, v)| (c.id.clone(), v.tx.clone(), v.hash.clone())).collect::<Vec<_>>(),
            "slice_members as of {asof}"
        );
        let vs = s.slice_values(&sl, &asof).unwrap();
        assert_eq!(
            vs.iter()
                .map(|m| (m.version.cell_id.clone(), m.version.receipt_hash.clone(), m.value.lexical().to_string()))
                .collect::<Vec<_>>(),
            want.iter().map(|(c, v)| (c.id.clone(), v.hash.clone(), v.value.clone())).collect::<Vec<_>>(),
            "slice_values as of {asof}"
        );
        for v in &vs {
            let c = cells.iter().find(|c| c.id == v.version.cell_id).unwrap();
            assert_eq!(
                (lex(&v.cell, "a"), lex(&v.cell, "b"), lex(&v.cell, "c")),
                (c.a.clone(), c.b.to_string(), c.c.to_string())
            );
        }

        // Changed after: a matching cell has a version with tx > A or asof > A.
        let changed = cells
            .iter()
            .filter(|c| spec.matches(c))
            .any(|c| c.versions.iter().any(|v| v.tx.as_str() > asof.as_str() || v.asof.as_str() > asof.as_str()));
        assert_eq!(s.slice_changed_after(&sl, &asof).unwrap(), changed, "slice_changed_after {asof}");

        // Computed cells: matching cells with a computed version answering, ascending by id.
        let mut comp: Vec<&C> = cells
            .iter()
            .filter(|c| spec.matches(c))
            .filter(|c| {
                c.computed.as_ref().is_some_and(|v| v.tx.as_str() <= asof.as_str() && v.asof.as_str() <= asof.as_str())
            })
            .collect();
        comp.sort_by(|x, y| x.id.cmp(&y.id));
        let got = s.computed_cells(&sl, &asof, 10_000).unwrap();
        assert_eq!(
            got.iter().map(|c| c.id()).collect::<Vec<_>>(),
            comp.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            "computed_cells {asof}"
        );
        let limit = comp.len().min(3);
        let got = s.computed_cells(&sl, &asof, limit).unwrap();
        assert_eq!(
            got.iter().map(|c| c.id()).collect::<Vec<_>>(),
            comp[..limit].iter().map(|c| c.id.clone()).collect::<Vec<_>>()
        );
    }
    assert_eq!(any, expect_nonempty, "{}", spec.json(h));
}

fn two_value_arguments_and_one_open(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let (h, cells) = world(&s);
    db.analyze();
    // The driving value constraint has candidates the other value constraint excludes.
    for (a, bb) in [("e1", 2), ("e5", 4), ("e0", 1)] {
        check(&s, &h, &cells, &Spec { a: Con::Val(a.into()), b: Con::Val(bb), c: Con::Open }, true);
    }
    check(&s, &h, &cells, &Spec { a: Con::Open, b: Con::Val(3), c: Con::Val(5) }, true);
    check(&s, &h, &cells, &Spec { a: Con::Val("e3".into()), b: Con::Open, c: Con::Val(2) }, true);
}

fn a_value_a_range_and_an_open_argument(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let (h, cells) = world(&s);
    db.analyze();
    check(&s, &h, &cells, &Spec { a: Con::Open, b: Con::Val(3), c: Con::Range(2, 4) }, true);
    check(&s, &h, &cells, &Spec { a: Con::Val("e2".into()), b: Con::Range(2, 3), c: Con::Open }, true);
    check(&s, &h, &cells, &Spec { a: Con::Range("e1".into(), "e3".into()), b: Con::Open, c: Con::Val(5) }, true);
    // An empty range.
    check(&s, &h, &cells, &Spec { a: Con::Open, b: Con::Val(3), c: Con::Range(4, 2) }, false);
    check(&s, &h, &cells, &Spec { a: Con::Open, b: Con::Val(3), c: Con::Range(6, 9) }, false);
}

fn a_value_no_cell_matches(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let (h, cells) = world(&s);
    db.analyze();
    check(&s, &h, &cells, &Spec { a: Con::Val("nope".into()), b: Con::Val(1), c: Con::Open }, false);
    check(&s, &h, &cells, &Spec { a: Con::Open, b: Con::Val(99), c: Con::Open }, false);
    check(&s, &h, &cells, &Spec { a: Con::Val("e1".into()), b: Con::Val(99), c: Con::Open }, false);
    check(&s, &h, &cells, &Spec { a: Con::Val("e1".into()), b: Con::Val(2), c: Con::Range(6, 9) }, false);
    // A value of another type than the argument's.
    let sl = slice(&format!(
        r#"{{"args":{{"a":{{"type":"number","value":"1"}},"b":{{"type":"number","value":"2"}},"c":"*"}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
    ));
    assert!(s.slice_members(&sl, &day(6)).unwrap().is_empty());
    assert!(s.slice_values(&sl, &day(6)).unwrap().is_empty());
    assert!(s.computed_cells(&sl, &day(6), 100).unwrap().is_empty());
    assert!(!s.slice_changed_after(&sl, &day(1)).unwrap());
}

backends!(two_value_arguments_and_one_open, a_value_a_range_and_an_open_argument, a_value_no_cell_matches,);
