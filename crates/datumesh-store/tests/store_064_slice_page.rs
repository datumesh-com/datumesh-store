//! Store 0.6.4, CONTRACT-0.5.1 S2: `Store::slice_page`, a page of a slice in sort-key order.
//! Both backends, plus PostgreSQL-only plan tests.
//!
//! ASSUMED API (additive; see store_064_sort_key.rs for the schema assumptions):
//!
//! ```text
//! // datumesh_store::SliceValue gains:  pub sort_key: Vec<u8>   (the member's cells.sort_key)
//! // on Store (a default body exists; the stores override it with the indexed query):
//! fn slice_page(&self, slice: &Slice, asof: &str, after: Option<&[u8]>, limit: usize)
//!     -> Result<Vec<SliceValue>>;
//! ```
//!
//! - Members whose sort key is strictly greater than `after` (all when `None`), in ascending
//!   sort-key order, at most `limit` (`limit == 0` is an empty page, not an error).
//! - The same membership rules as `slice_values`: fixed arguments, ranges (inclusive), the
//!   latest version answering as of `asof` (tx <= asof and receipt asof <= asof), a withdrawn
//!   cell left out (and not counted against `limit`).
//! - The order is `slice_values`' order, exactly: the page walk equals it member for member.
//! - The next cursor is the last member's `sort_key`.
//! - PostgreSQL: the page is read through the `(fn_hash, ns, fn, sort_key)` index with no
//!   Sort over the members (checked by the index's scan counter after a real `slice_page`,
//!   and by EXPLAIN of the page's shape with sorts disabled).

#[macro_use]
mod common;

use std::time::{Duration, Instant};

use common::*;
use datumesh_core::{Receipt, ReceiptBuilder, Scalar, Type, Withdrawal};
use datumesh_store::{CellWrite, SliceValue};

const T: &str = "2024-01-01T00:00:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";

fn slice_of(h: &str, f: &str, args: &[(&str, &str)]) -> datumesh_core::Slice {
    let a: Vec<String> = args.iter().map(|(n, s)| format!(r#""{n}":{s}"#)).collect();
    slice(&format!(r#"{{"args":{{{}}},"fn":"{f}","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#, a.join(",")))
}

const OPEN: &str = r#""*""#;

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

/// Every page of `limit`, from the start, following the last member's key; every page is
/// full except the last, and an empty page follows the end.
fn walk(s: &TStore, sl: &datumesh_core::Slice, asof: &str, limit: usize) -> Vec<SliceValue> {
    let s = s.st();
    let mut all: Vec<SliceValue> = vec![];
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = s.slice_page(sl, asof, after.as_deref(), limit).unwrap();
        assert!(page.len() <= limit);
        let Some(last) = page.last() else { break };
        after = Some(last.sort_key.clone());
        let full = page.len() == limit;
        all.extend(page);
        if !full {
            // The end: nothing after the last key.
            assert!(s.slice_page(sl, asof, after.as_deref(), limit).unwrap().is_empty());
            break;
        }
    }
    all
}

/// The parts of a member that `slice_values` also gives.
fn plain(v: &[SliceValue]) -> Vec<(datumesh_store::Version, Scalar, datumesh_core::Cell)> {
    v.iter().map(|m| (m.version.clone(), m.value.clone(), m.cell.clone())).collect()
}

fn same_as_slice_values(s: &TStore, sl: &datumesh_core::Slice, asof: &str, limit: usize) -> Vec<SliceValue> {
    let page = walk(s, sl, asof, limit);
    let all = s.st().slice_values(sl, asof).unwrap();
    assert_eq!(plain(&page), plain(&all), "the page walk is slice_values, member for member and in order");
    for w in page.windows(2) {
        assert!(w[0].sort_key < w[1].sort_key);
    }
    page
}

fn nums(v: &[SliceValue]) -> Vec<String> {
    v.iter().map(|m| m.value.lexical().to_string()).collect()
}

fn hundredths(h: i64) -> String {
    let (sign, a) = (if h < 0 { "-" } else { "" }, h.unsigned_abs());
    let (i, f) = (a / 100, a % 100);
    match f {
        0 => format!("{sign}{i}"),
        f if f % 10 == 0 => format!("{sign}{i}.{}", f / 10),
        f => format!("{sign}{i}.{f:02}"),
    }
}

// ---- The 10,000-cell worlds -----------------------------------------------------------------

/// `f(a: number, b: string)`, 100 x 100 cells; the value of a cell is a unique id. Returns the
/// store, the hash, and the ids in the key order (a numerically, then b).
fn two_open_world(db: &TestDb) -> (TStore, String, Vec<String>) {
    let s = slice_store(db);
    let h = s.put_descriptor(&stored_descriptor(&[("a", Type::Number), ("b", Type::String)], "f")).unwrap();
    let mut rows: Vec<(i64, String, String)> = vec![];
    for i in 0..100i64 {
        for j in 0..100i64 {
            // a: hundredths from -1250 in steps of 25 (negative, zero, 1- and 2-decimal forms);
            // b: zero-padded, with an upper-case run that sorts before the lower-case ones.
            let b = if j % 10 == 0 { format!("B{j:03}") } else { format!("b{j:03}") };
            rows.push(((i - 50) * 25, b, format!("{}", i * 100 + j + 1)));
        }
    }
    // Insert in a scrambled order.
    let n = rows.len();
    let scrambled: Vec<&(i64, String, String)> = (0..n).map(|i| &rows[(i * 7919) % n]).collect();
    let cells: Vec<Vec<(&str, Scalar)>> = scrambled
        .iter()
        .map(|(a, b, _)| vec![("a", sc(Type::Number, &hundredths(*a))), ("b", sc(Type::String, b))])
        .collect();
    let values: Vec<String> = scrambled.iter().map(|r| r.2.clone()).collect();
    put_all(&s, "f", &h, &cells, &values);
    let mut sorted = rows.clone();
    sorted.sort_by(|x, y| (x.0, x.1.as_bytes()).cmp(&(y.0, y.1.as_bytes())));
    (s, h, sorted.into_iter().map(|r| r.2).collect())
}

fn a_page_walk_of_10000_cells_with_two_open_arguments(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h, want) = two_open_world(&db);
    assert_eq!(db.count("cells"), 10_000);
    let sl = slice_of(&h, "f", &[("a", OPEN), ("b", OPEN)]);
    let got = same_as_slice_values(&s, &sl, END, 777);
    assert_eq!(nums(&got), want, "every member once, in key order");
    // A page size that divides the total, and one larger than it.
    assert_eq!(nums(&walk(&s, &sl, END, 500)), want);
    assert_eq!(nums(&walk(&s, &sl, END, 20_000)), want);
    assert_eq!(nums(&walk(&s, &sl, END, 1)[..3]), want[..3]);
    // Cursors from the middle.
    let mid = &got[4_999];
    let rest = s.slice_page(&sl, END, Some(&mid.sort_key), 3).unwrap();
    assert_eq!(nums(&rest), want[5_000..5_003]);
    let first = s.slice_page(&sl, END, None, 3).unwrap();
    assert_eq!(nums(&first), want[..3]);
    assert_eq!(s.slice_page(&sl, END, None, 0).unwrap().len(), 0);
    let last = got.last().unwrap();
    assert!(s.slice_page(&sl, END, Some(&last.sort_key), 10).unwrap().is_empty());
}

/// `f(g: string, n: number)`: 4 groups x 2,500 numbers.
fn fixed_open_world(db: &TestDb) -> (TStore, String, Vec<Vec<(i64, String)>>) {
    let s = slice_store(db);
    let h = s.put_descriptor(&stored_descriptor(&[("g", Type::String), ("n", Type::Number)], "f")).unwrap();
    let mut rows: Vec<(String, i64, String)> = vec![];
    let mut per_group: Vec<Vec<(i64, String)>> = vec![vec![]; 4];
    for g in 0..4i64 {
        for j in 0..2500i64 {
            let n = (j - 1000) * 7; // hundredths: -7000 .. +10443, zero included
            let id = format!("{}", g * 10_000 + j + 1);
            rows.push((format!("g{g}"), n, id.clone()));
            per_group[g as usize].push((n, id));
        }
    }
    for g in &mut per_group {
        g.sort();
    }
    let n = rows.len();
    let scrambled: Vec<&(String, i64, String)> = (0..n).map(|i| &rows[(i * 7919) % n]).collect();
    let cells: Vec<Vec<(&str, Scalar)>> = scrambled
        .iter()
        .map(|(g, n, _)| vec![("g", sc(Type::String, g)), ("n", sc(Type::Number, &hundredths(*n)))])
        .collect();
    let values: Vec<String> = scrambled.iter().map(|r| r.2.clone()).collect();
    put_all(&s, "f", &h, &cells, &values);
    (s, h, per_group)
}

fn a_page_walk_of_10000_cells_with_one_fixed_and_one_open_argument(b: Backend) {
    let db = TestDb::migrated(b);
    let (s, h, groups) = fixed_open_world(&db);
    assert_eq!(db.count("cells"), 10_000);
    // The fixed argument sorts before the open one.
    for g in [0usize, 2] {
        let sl = slice_of(&h, "f", &[("g", &format!(r#"{{"type":"string","value":"g{g}"}}"#)), ("n", OPEN)]);
        let got = same_as_slice_values(&s, &sl, END, 333);
        let want: Vec<String> = groups[g].iter().map(|r| r.1.clone()).collect();
        assert_eq!(got.len(), 2500);
        assert_eq!(nums(&got), want, "group g{g}: every member once, by n numerically");
        assert_eq!(nums(&walk(&s, &sl, END, 2500)), want);
    }
    // The fixed argument sorts after the open one: `g` open, `n` fixed (n = 0.07 is in every group).
    let sl = slice_of(&h, "f", &[("g", OPEN), ("n", r#"{"type":"number","value":"0.07"}"#)]);
    let got = same_as_slice_values(&s, &sl, END, 3);
    let want: Vec<String> = (0..4).map(|g| groups[g].iter().find(|r| r.0 == 7).unwrap().1.clone()).collect();
    assert_eq!(nums(&got), want);
    // Both open: the whole 10,000, g then n.
    let all = slice_of(&h, "f", &[("g", OPEN), ("n", OPEN)]);
    let got = same_as_slice_values(&s, &all, END, 1234);
    let want: Vec<String> = groups.iter().flat_map(|g| g.iter().map(|r| r.1.clone())).collect();
    assert_eq!(nums(&got), want);
}

// ---- Ranges, withdrawals, asof ----------------------------------------------------------------

fn a_range_slice_pages_its_members_inclusively(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("d", Type::Date), ("k", Type::Number)], "f")).unwrap();
    let dates = ["2024-01-01", "2024-01-02", "2024-01-03", "2024-01-04", "2024-01-05"];
    let mut rows: Vec<(String, i64)> = vec![];
    for d in dates {
        for k in -10..=10i64 {
            rows.push((d.to_string(), k * 50 + 5)); // hundredths: -495 .. 505, i.e. -4.95 ... 5.05
        }
    }
    let cells: Vec<Vec<(&str, Scalar)>> =
        rows.iter().map(|(d, k)| vec![("d", sc(Type::Date, d)), ("k", sc(Type::Number, &hundredths(*k)))]).collect();
    let values: Vec<String> = (0..rows.len()).map(|i| i.to_string()).collect();
    put_all(&s, "f", &h, &cells, &values);
    // d in [2024-01-02, 2024-01-04], k in [-2.45, 3.05] (both bounds are values present).
    let sl = slice_of(
        &h,
        "f",
        &[
            ("d", r#"{"hi":"2024-01-04","lo":"2024-01-02","type":"date"}"#),
            ("k", r#"{"hi":"3.05","lo":"-2.45","type":"number"}"#),
        ],
    );
    let mut want: Vec<(String, i64, usize)> = rows
        .iter()
        .enumerate()
        .filter(|(_, (d, k))| d.as_str() >= "2024-01-02" && d.as_str() <= "2024-01-04" && *k >= -245 && *k <= 305)
        .map(|(i, (d, k))| (d.clone(), *k, i))
        .collect();
    want.sort();
    assert_eq!(want.len(), 3 * 12, "bounds inclusive: 12 values of k from -2.45 to 3.05");
    let got = same_as_slice_values(&s, &sl, END, 5);
    assert_eq!(nums(&got), want.iter().map(|w| w.2.to_string()).collect::<Vec<_>>());
    // A range with an open argument: the first page after a cursor inside the range.
    let half = slice_of(&h, "f", &[("d", r#"{"hi":"2024-01-04","lo":"2024-01-02","type":"date"}"#), ("k", OPEN)]);
    let all = same_as_slice_values(&s, &half, END, 8);
    assert_eq!(all.len(), 3 * 21);
    let after = s.slice_page(&half, END, Some(&all[30].sort_key), 4).unwrap();
    assert_eq!(plain(&after), plain(&all[31..35]));
    // A range that holds nothing is an empty page.
    let none = slice_of(&h, "f", &[("d", r#"{"hi":"2030-01-02","lo":"2030-01-01","type":"date"}"#), ("k", OPEN)]);
    assert!(s.slice_page(&none, END, None, 10).unwrap().is_empty());
}

fn the_page_answers_as_of_asof_like_slice_values(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap();
    let day = |d: u32| format!("2024-01-{d:02}T00:00:00.000000Z");
    let cell = |k: &str| slice_cell("f", &h, &[("k", sc(Type::Number, k))]);
    // k=1: v1 at day 2, a correction at day 6; k=2 late-signed (tx 3, asof 8); k=3 from day 10.
    put_member(&s, &cell("1"), "100", &day(2), &day(2));
    put_member(&s, &cell("1"), "101", &day(6), &day(6));
    put_member(&s, &cell("2"), "200", &day(3), &day(8));
    put_member(&s, &cell("3"), "300", &day(10), &day(10));
    let sl = slice_of(&h, "f", &[("k", OPEN)]);
    let vals = |a: &str, limit: usize| nums(&same_as_slice_values(&s, &sl, a, limit));
    assert!(vals(&day(1), 2).is_empty());
    assert_eq!(vals(&day(2), 2), ["100"]);
    assert_eq!(vals(&day(5), 1), ["100"]);
    assert_eq!(vals(&day(6), 1), ["101"]);
    assert_eq!(vals(&day(8), 1), ["101", "200"]);
    assert_eq!(vals(END, 2), ["101", "200", "300"]);
    // A cursor between members is a cursor: the keys of members absent as of asof still order.
    let all = s.slice_page(&sl, END, None, 10).unwrap();
    let at5 = s.slice_page(&sl, &day(5), Some(&all[0].sort_key), 10).unwrap();
    assert!(at5.is_empty(), "k=2 and k=3 are not members as of day 5");
    let at8 = s.slice_page(&sl, &day(8), Some(&all[0].sort_key), 10).unwrap();
    assert_eq!(nums(&at8), ["200"]);
}

const T0: &str = "2024-03-10T12:00:00.000000Z";
const TW: &str = "2024-03-15T16:00:00.000000Z";
const AW: &str = "2024-03-16T09:00:00.000000Z";
const T2: &str = "2024-03-20T08:30:00.000000Z";

/// A receipt of the fx_rate cell EUR/`quote`/2024-03-15, signed with the vector's key.
fn quote_receipt(quote: &str, value: &str, tx: &str) -> (String, Vec<u8>) {
    let v = vector("receipt/stored-number");
    let base = Receipt::from_json(&input(&v)).unwrap();
    let text = String::from_utf8(base.cell().canonical()).unwrap();
    assert!(text.contains(r#""value":"USD""#));
    let cell =
        datumesh_core::Cell::from_json(text.replace(r#""value":"USD""#, &format!(r#""value":"{quote}""#)).as_bytes())
            .unwrap();
    let r = ReceiptBuilder::new()
        .cell(cell.clone())
        .value(sc(Type::Number, value))
        .valid(base.valid().clone())
        .asof(tx)
        .tx(tx)
        .issued(tx)
        .publisher(base.publisher())
        .signer_host(base.signer_host())
        .licence(base.licence())
        .summary(format!("1 EUR was worth {value} {quote} at the 15 March 2024 reference rate."))
        .sign(&signing_key(&v))
        .unwrap();
    (cell.id(), receipt_json(&r))
}

fn a_withdrawn_member_is_left_out_and_not_counted(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    let fh = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    for (q, v) in [("CHF", "1"), ("GBP", "2"), ("JPY", "3"), ("USD", "4")] {
        let (id, bytes) = quote_receipt(q, v, T0);
        let h = s.put_receipt(&bytes).unwrap();
        s.put_version(&id, T0, &h).unwrap();
    }
    let usd = quote_receipt("USD", "4", T0).0;
    let wbytes = input(&vector("receipt/withdrawn-primary"));
    let w = Withdrawal::from_json(&wbytes).unwrap();
    assert_eq!(w.cell().id(), usd, "the vector withdraws the USD cell");
    let wh = s.put_receipt(&wbytes).unwrap();
    s.put_version(&usd, TW, &wh).unwrap();
    let sl = slice(&format!(
        r#"{{"args":{{"base":{{"type":"string","value":"EUR"}},"date":{{"type":"date","value":"2024-03-15"}},"quote":"*"}},"fn":"fx_rate","fn_hash":"{fh}","ns":"rates.example.org"}}"#
    ));
    // Before the withdrawal's asof all four (C collation: CHF GBP JPY USD).
    assert_eq!(nums(&same_as_slice_values(&s, &sl, T0, 3)), ["1", "2", "3", "4"]);
    // From its asof on: three, and a page of two does not stop short for the withdrawn one.
    let pages = |limit: usize| -> Vec<Vec<String>> {
        let mut out = vec![];
        let mut after: Option<Vec<u8>> = None;
        loop {
            let p = s.slice_page(&sl, END, after.as_deref(), limit).unwrap();
            if p.is_empty() {
                break;
            }
            after = Some(p.last().unwrap().sort_key.clone());
            out.push(nums(&p));
        }
        out
    };
    assert_eq!(nums(&same_as_slice_values(&s, &sl, AW, 2)), ["1", "2", "3"]);
    assert_eq!(pages(2), [vec!["1", "2"], vec!["3"]]);
    assert_eq!(pages(3), [vec!["1", "2", "3"]]);
    assert_eq!(pages(1), [vec!["1"], vec!["2"], vec!["3"]]);
    // A later valued version ends the withdrawal: USD is last again.
    let (id, bytes) = quote_receipt("USD", "5", T2);
    let h2 = s.put_receipt(&bytes).unwrap();
    s.put_version(&id, T2, &h2).unwrap();
    assert_eq!(nums(&same_as_slice_values(&s, &sl, END, 2)), ["1", "2", "3", "5"]);
}

backends!(
    a_page_walk_of_10000_cells_with_two_open_arguments,
    a_page_walk_of_10000_cells_with_one_fixed_and_one_open_argument,
    a_range_slice_pages_its_members_inclusively,
    the_page_answers_as_of_asof_like_slice_values,
    a_withdrawn_member_is_left_out_and_not_counted,
);

// ---- PostgreSQL: an index scan, no Sort ---------------------------------------------------------

fn sort_key_index_scans(db: &PgDb) -> (i64, i64) {
    let mut c = db.client();
    let r = c
        .query_one(
            "SELECT COALESCE((SELECT sum(idx_scan) FROM pg_stat_user_indexes
                              WHERE relname = 'cells' AND indexrelname IN
                                (SELECT indexname FROM pg_indexes WHERE tablename = 'cells' AND indexdef LIKE '%sort_key%')), 0)::bigint,
                    (SELECT seq_scan FROM pg_stat_user_tables WHERE relname = 'cells')::bigint",
            &[],
        )
        .unwrap();
    (r.get(0), r.get(1))
}

/// A real `slice_page` on 10,000 cells is read through the sort-key index: the index's scan
/// counter goes up and `cells` is not scanned sequentially.
#[test]
fn pg_slice_page_reads_through_the_sort_key_index() {
    let db = TestDb::migrated(Backend::Pg);
    let (s, h, groups) = fixed_open_world(&db);
    db.analyze();
    let sl = slice_of(&h, "f", &[("g", r#"{"type":"string","value":"g1"}"#), ("n", OPEN)]);
    let open_both = slice_of(&h, "f", &[("g", OPEN), ("n", OPEN)]);
    // Let the statistics of the setup settle, then count.
    std::thread::sleep(Duration::from_millis(1500));
    let (idx0, seq0) = sort_key_index_scans(db.pg());
    let started = Instant::now();
    let p1 = s.slice_page(&sl, END, None, 100).unwrap();
    let p2 = s.slice_page(&sl, END, Some(&p1.last().unwrap().sort_key), 100).unwrap();
    let p3 = s.slice_page(&open_both, END, None, 100).unwrap();
    assert_eq!((p1.len(), p2.len(), p3.len()), (100, 100, 100));
    assert_eq!(p1[0].value.lexical(), groups[1][0].1);
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut now = sort_key_index_scans(db.pg());
    while now.0 <= idx0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        now = sort_key_index_scans(db.pg());
    }
    assert!(now.0 > idx0, "the sort_key index was scanned by slice_page ({idx0} -> {})", now.0);
    assert_eq!(now.1, seq0, "cells was not scanned sequentially by slice_page");
}

/// The shape of the page query over the index: an index scan in key order with a LIMIT, and
/// no Sort, even with sorting disabled (so a Sort could only appear if the index could not
/// give the order). The text is the page's shape, not the store's SQL.
#[test]
fn pg_explain_of_the_page_shape_is_an_index_scan_without_a_sort() {
    let db = TestDb::migrated(Backend::Pg);
    let (_s, h, _g) = fixed_open_world(&db);
    db.analyze();
    let mut c = db.pg().client();
    c.batch_execute("SET enable_seqscan = off; SET enable_sort = off; SET enable_bitmapscan = off").unwrap();
    let q = format!(
        "EXPLAIN SELECT cell_id FROM cells
         WHERE fn_hash = '{h}' AND ns = '{SLICE_HOST}' AND fn = 'f' AND sort_key > '\\x00'::bytea
         ORDER BY sort_key LIMIT 100"
    );
    let plan: Vec<String> = c.query(q.as_str(), &[]).unwrap().iter().map(|r| r.get::<_, String>(0)).collect();
    let text = plan.join("\n");
    assert!(text.contains("Index"), "{text}");
    assert!(!text.contains("Sort"), "no Sort node over the members:\n{text}");
    assert!(!text.contains("Seq Scan"), "{text}");
    // The index the plan uses is the sort-key one.
    assert!(
        db.texts("SELECT indexname FROM pg_indexes WHERE tablename = 'cells' AND indexdef LIKE '%sort_key%'")
            .iter()
            .any(|r| text.contains(r[0].as_str())),
        "{text}"
    );
}
