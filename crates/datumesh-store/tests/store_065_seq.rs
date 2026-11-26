//! Store 0.6.5, "G3 asks 'inserted since', not 'recorded after'" (CONTRACT-guard-patch.md, decided
//! 26 Nov): a store-assigned insertion sequence on every version and withdrawal, the highest one a
//! slice matches, and the member-set row's `seen_seq`. Both backends. Failing tests: not written yet.
//!
//! API assumed:
//!
//! ```text
//! pub struct Version { ..., pub seq: i64 }        // the insertion sequence: only increases
//! fn slice_seq(&self, slice: &Slice) -> Result<i64>;
//!     // the highest seq among the versions (withdrawals included) of the cells the slice matches
//!     // (its function, ns, fixed and ranged arguments, as slice_values matches them; for a version
//!     // slice, its cell's versions), whatever their recorded time; 0 when none
//! pub struct MemberSet { ..., pub seen_seq: i64 } // the slice's seq read before its members
//! fn fn_seq(&self, fn_hash: &str) -> Result<i64>;
//!     // the highest seq of any version of the function's cells, 0 when none; >= slice_seq of every
//!     // slice of the function (decided 26 Nov: the engine's quick check before slice_seq)
//! ```
//!
//! Migration 17 / 13 numbers the existing rows in insertion order (here: the order of the rows'
//! physical insertion, which for these tests is tx order per cell and cell order as written).
//! An index-backed plan for slice_seq is the implementer's (its SQL is not known to the tests; the
//! plan checks of src/sqlite.rs's unit tests are the place for it).

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Cell, Manifest, Type};
use datumesh_store::{migrate, MemberSet, SqliteOptions};

const T1: &str = "2024-01-01T00:00:00.000000Z";
const T2: &str = "2024-01-02T00:00:00.000000Z";
const T3: &str = "2024-01-03T00:00:00.000000Z";

fn function(s: &TStore, body: &str) -> String {
    s.put_descriptor(&stored_descriptor(&[("k", Type::Number), ("tenor", Type::Number)], body)).unwrap()
}

fn cell(h: &str, k: u32, tenor: u32) -> Cell {
    slice_cell("f", h, &[("k", sc(Type::Number, &k.to_string())), ("tenor", sc(Type::Number, &tenor.to_string()))])
}

/// f(k: *, tenor: 1): every k at tenor 1.
fn tenor1(h: &str) -> datumesh_core::Slice {
    slice(&format!(
        r#"{{"args":{{"k":"*","tenor":{{"type":"number","value":"1"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
    ))
}

fn seq_of(s: &TStore, c: &Cell) -> i64 {
    s.latest(&c.id()).unwrap().unwrap().seq
}

fn every_version_gets_an_increasing_seq(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s, "f");
    let mut last = 0;
    // Out of tx order across cells: the seq follows insertion, not the signed or recorded time.
    for (k, tx) in [(1, T3), (2, T1), (3, T2)] {
        put_member(&s, &cell(&h, k, 1), "1", tx, tx);
        let q = seq_of(&s, &cell(&h, k, 1));
        assert!(q > last, "k {k}: {q} after {last}");
        last = q;
    }
    // A second version of a cell: a later seq; the first keeps its own.
    let first = seq_of(&s, &cell(&h, 2, 1));
    put_member(&s, &cell(&h, 2, 1), "2", T2, T2);
    let second = seq_of(&s, &cell(&h, 2, 1));
    assert!(second > last);
    let vs = s.versions(&cell(&h, 2, 1).id(), T1, T3).unwrap();
    assert_eq!(vs.iter().map(|v| v.seq).collect::<Vec<_>>(), [first, second]);
    // put_cells numbers its versions in order too.
    let (c4, c5) = (cell(&h, 4, 1), cell(&h, 5, 1));
    let (r4, r5) = (slice_receipt(&c4, "4", T1, T1), slice_receipt(&c5, "5", T1, T1));
    let (i4, i5) = (c4.id(), c5.id());
    s.put_cells(&[
        datumesh_store::CellWrite::new(&r4, Some((i4.as_str(), T1))),
        datumesh_store::CellWrite::new(&r5, Some((i5.as_str(), T1))),
    ])
    .unwrap();
    assert!(seq_of(&s, &c4) > second && seq_of(&s, &c5) > seq_of(&s, &c4));
    // The column is the database's: it cannot be updated.
    db.refused("UPDATE versions SET seq = seq + 1000");
}

fn slice_seq_rises_on_a_new_member_and_a_backdated_correction_and_not_outside_the_slice(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s, "f");
    let sl = tenor1(&h);
    assert_eq!(s.slice_seq(&sl).unwrap(), 0, "no version: 0");
    put_member(&s, &cell(&h, 1, 1), "1", T2, T2);
    let a = s.slice_seq(&sl).unwrap();
    assert_eq!(a, seq_of(&s, &cell(&h, 1, 1)));
    // Another tenor, and another function with the same arguments: the slice's seq does not move.
    put_member(&s, &cell(&h, 1, 2), "9", T3, T3);
    let g = function(&s, "g");
    put_member(
        &s,
        &slice_cell("f", &g, &[("k", sc(Type::Number, "1")), ("tenor", sc(Type::Number, "1"))]),
        "9",
        T3,
        T3,
    );
    assert_eq!(s.slice_seq(&sl).unwrap(), a, "versions outside the slice");
    // A new member, recorded (signed) earlier than the first: the seq rises.
    put_member(&s, &cell(&h, 2, 1), "2", T1, T1);
    let b2 = s.slice_seq(&sl).unwrap();
    assert!(b2 > a, "a new member, whatever its time");
    // A correction of a member (no recording clock): recorded at its signed T3, earlier than
    // nothing here but not later than other versions' times either; the seq rises again.
    put_member(&s, &cell(&h, 1, 1), "1.5", T3, T3);
    let c = s.slice_seq(&sl).unwrap();
    assert!(c > b2, "a correction");
    assert_eq!(c, seq_of(&s, &cell(&h, 1, 1)));
}

fn slice_seq_rises_on_a_withdrawal(b: Backend) {
    // The world of formula-eval/withdrawn-slice-count (see store_060_withdrawal_slice): ten daily
    // readings; their withdrawal of 4 March is written last.
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    put_vector_keys(&s, &vector("receipt/slice-join"));
    let v = vector("formula-eval/withdrawn-slice-count");
    let w = v.get("world").unwrap();
    let mut fn_hash = String::new();
    for (name, f) in w.get("functions").and_then(|f| f.as_object()).unwrap() {
        let h = s.put_descriptor(&f.get("descriptor").unwrap().to_jcs()).unwrap();
        if name == "data.example.org/daily" {
            fn_hash = h;
        }
    }
    let receipts = match w.get("receipts").unwrap() {
        datumesh_core::json::Value::Array(a) => a.clone(),
        _ => unreachable!(),
    };
    let (valued, withdrawals): (Vec<_>, Vec<_>) = receipts.into_iter().partition(|r| r.get("withdrawn").is_none());
    assert_eq!(withdrawals.len(), 1);
    for r in valued.iter().chain(withdrawals.iter()) {
        if r.get("withdrawn").is_some() {
            let sl = slice(&format!(
                r#"{{"args":{{"date":"*"}},"fn":"daily","fn_hash":"{fn_hash}","ns":"data.example.org"}}"#
            ));
            let before = s.slice_seq(&sl).unwrap();
            assert!(before > 0);
            let cell = Cell::from_value(r.get("cell").unwrap()).unwrap();
            let h = s.put_receipt(&r.to_jcs()).unwrap();
            s.put_version(&cell.id(), str_of(r, "tx"), &h).unwrap();
            let after = s.slice_seq(&sl).unwrap();
            assert!(after > before, "a withdrawal raises the slice's seq: {before} -> {after}");
            assert_eq!(after, s.latest(&cell.id()).unwrap().unwrap().seq);
        } else {
            let cell = Cell::from_value(r.get("cell").unwrap()).unwrap();
            let h = s.put_receipt(&r.to_jcs()).unwrap();
            s.put_version(&cell.id(), str_of(r, "tx"), &h).unwrap();
        }
    }
}

fn the_member_set_row_keeps_seen_seq(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s, "f");
    let m = Manifest::new(tenor1(&h), vec![]).unwrap();
    let mut set = MemberSet::from_manifest(&m, None, T2);
    assert_eq!(set.seen_seq, 0, "from_manifest leaves it 0; the engine sets it");
    set.seen_seq = 41;
    assert!(s.put_member_set(&set).unwrap());
    assert_eq!(s.member_set(&m.entry()).unwrap().unwrap().seen_seq, 41);
    assert_eq!(db.int("SELECT seen_seq FROM member_sets"), 41);
}

fn migration_17_numbers_existing_rows_in_insertion_order(b: Backend) {
    let (old, new) = if b == Backend::Pg { (16, 17) } else { (12, 13) };
    let db = TestDb::empty(b);
    migrate_to(&db, b, old);
    let s = slice_store(&db);
    let h = function(&s, "f");
    let order = [(1, T3), (2, T1), (1, "2024-01-04T00:00:00.000000Z"), (3, T2)];
    for (k, tx) in order {
        put_member(&s, &cell(&h, k, 1), "1", tx, tx);
    }
    drop(s);
    assert_eq!(apply_rest(&db, b), [new]);
    let s = db.store();
    let seqs: Vec<i64> = [(1, T3), (2, T1), (1, "2024-01-04T00:00:00.000000Z"), (3, T2)]
        .iter()
        .map(|(k, tx)| s.versions(&cell(&h, *k, 1).id(), tx, tx).unwrap()[0].seq)
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "numbered in insertion order: {seqs:?}");
    assert!(seqs[0] > 0);
    // A version written after the migration follows them.
    put_member(&s, &cell(&h, 9, 1), "1", T1, T1);
    assert!(seq_of(&s, &cell(&h, 9, 1)) > seqs[3]);
    assert_eq!(s.slice_seq(&tenor1(&h)).unwrap(), seq_of(&s, &cell(&h, 9, 1)));
}

fn fn_seq_rises_on_any_version_of_the_function_and_bounds_its_slices(b: Backend) {
    // Store::fn_seq(fn_hash): the highest seq of any version of the function, 0 with none; the
    // engine's quick check before slice_seq.
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s, "f");
    let g = function(&s, "g");
    assert_eq!(s.fn_seq(&h).unwrap(), 0, "no version: 0");
    let gcell =
        |k: u32| slice_cell("f", &g, &[("k", sc(Type::Number, &k.to_string())), ("tenor", sc(Type::Number, "1"))]);
    let check = |what: &str| {
        let f = s.fn_seq(&h).unwrap();
        for sl in [tenor1(&h), tenor2(&h)] {
            assert!(f >= s.slice_seq(&sl).unwrap(), "{what}: fn_seq bounds every slice of the function");
        }
        f
    };
    put_member(&s, &cell(&h, 1, 1), "1", T2, T2);
    let a = check("a member at tenor 1");
    assert_eq!(a, seq_of(&s, &cell(&h, 1, 1)));
    // Another tenor (outside tenor1's slice): fn_seq rises, tenor1's slice_seq does not.
    put_member(&s, &cell(&h, 1, 2), "2", T2, T2);
    let b2 = check("a member at tenor 2");
    assert!(b2 > a);
    assert_eq!(s.slice_seq(&tenor1(&h)).unwrap(), a);
    // Another function: fn_seq does not move.
    put_member(&s, &gcell(1), "3", T3, T3);
    assert_eq!(s.fn_seq(&h).unwrap(), b2, "a version of another function");
    assert_eq!(s.fn_seq(&g).unwrap(), seq_of(&s, &gcell(1)));
    // A correction signed earlier than everything (no recording clock): it rises.
    put_member(&s, &cell(&h, 1, 1), "1.5", T3, T3);
    assert!(check("a correction") > b2);
    assert!(matches!(s.fn_seq("not-a-hash"), Err(datumesh_store::Error::Invalid(_))));
}

/// f(k: *, tenor: 2).
fn tenor2(h: &str) -> datumesh_core::Slice {
    slice(&format!(
        r#"{{"args":{{"k":"*","tenor":{{"type":"number","value":"2"}}}},"fn":"f","fn_hash":"{h}","ns":"{SLICE_HOST}"}}"#
    ))
}

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

backends!(
    every_version_gets_an_increasing_seq,
    slice_seq_rises_on_a_new_member_and_a_backdated_correction_and_not_outside_the_slice,
    slice_seq_rises_on_a_withdrawal,
    the_member_set_row_keeps_seen_seq,
    migration_17_numbers_existing_rows_in_insertion_order,
    fn_seq_rises_on_any_version_of_the_function_and_bounds_its_slices,
);
