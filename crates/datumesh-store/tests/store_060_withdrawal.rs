//! Store 0.6.0, R1 (CONTRACT-round-0.7): a withdrawal is the null version of a stored cell.
//! Both backends (plus one PostgreSQL-only migration test).
//!
//! A withdrawal receipt (SPEC 8.1, `spec-0.7.0`: `withdrawn` in place of `type` and `value`,
//! `inputs` empty) is stored through the normal put path (`put_receipt`, `put_cells`, then
//! `put_version`), never panics, and is a version of its cell that says "no value". The
//! core has no signing testkit for withdrawals, so the tests use the signed spec vectors
//! `receipt/withdrawn-{primary,relayed,non-ascii,max-length}` (spec submodule at 99f2493): all four
//! withdraw the stored-number cell (rates.example.org fx_rate EUR/USD 2024-03-15) with tx
//! 2024-03-15T16:00 and asof 2024-03-16T09:00 (a late-signed withdrawal). The valued versions
//! around it are the harness's `stored_number_at` receipts of the same cell.
//!
//! API assumed (the smallest that fits the existing reads; all ADDITIVE):
//!
//! ```text
//! // datumesh_store::Version (already #[non_exhaustive]) gains a public field:
//! pub withdrawn: bool,   // true when the version's receipt is a withdrawal; false otherwise
//! ```
//!
//! - `put_receipt(withdrawal bytes)` verifies it as any stored receipt (signature, key
//!   document of the signer host) and stores the canonical bytes; it returns the withdrawal's
//!   hash (`datumesh_core::Withdrawal::hash`); `Resolver::receipt(hash)` returns the canonical
//!   bytes (`Withdrawal::canonical`), which `Receipt::from_json` refuses and
//!   `Withdrawal::from_json` accepts. Idempotent like any receipt.
//! - `put_version(cell_id, tx, withdrawal hash)` and `put_cells` accept it.
//! - `stored_asof(cell, A)`: by the existing answering rule (latest tx <= A whose receipt's
//!   asof is also <= A), so a withdrawal answers from its asof on: it returns that `Version`
//!   with `withdrawn == true` and `receipt_hash` the withdrawal's. Before then, the earlier
//!   valued version answers, `withdrawn == false`. A later valued version ends the withdrawal.
//! - `version_asof`, `versions`, `latest` list/answer it as a `Version` with `withdrawn == true`.
//! - `Resolver::evaluate(cell, A)` is `None` while the answering version is a withdrawal.
//! - `slice_values` leaves withdrawn cells out (no value to give); a member whose version
//!   is a withdrawal is simply not in the list, the other members are.
//! - `cell_count` / `stored_cell_count` do not change on a withdrawal of an existing cell.
//! - A database whose `receipts` table holds a withdrawal (NOT NULL value columns of
//!   migration 4 must not choke on a valueless receipt) migrates and stores.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Receipt, Resolver, Scalar, Type, Withdrawal};
use datumesh_store::{CellWrite, Version};

const T0: &str = "2024-03-10T12:00:00.000000Z";
const TW: &str = "2024-03-15T16:00:00.000000Z"; // the withdrawals' tx
const AW: &str = "2024-03-16T09:00:00.000000Z"; // the withdrawals' asof
const T2: &str = "2024-03-20T08:30:00.000000Z";
const MIDDLE: &str = "2024-03-15T20:00:00.000000Z"; // after the withdrawal's tx, before its asof
const AFTER: &str = "2024-03-17T00:00:00.000000Z";
const FIRST: &str = "0001-01-01T00:00:00.000000Z";
const END: &str = "9999-12-31T23:59:59.999999Z";
const IDS: [&str; 4] = [
    "receipt/withdrawn-primary",
    "receipt/withdrawn-relayed",
    "receipt/withdrawn-non-ascii",
    "receipt/withdrawn-max-length",
];

fn n(x: &str) -> Scalar {
    Scalar::from_lexical(Type::Number, x).unwrap()
}

struct World {
    db: TestDb,
    s: TStore,
    wbytes: Vec<u8>,
    w: Withdrawal,
    id: String,
    v0: String,
}

/// A store with the fx_rate descriptor, a valued version at T0 and the withdrawal's bytes
/// (not yet stored).
fn world(b: Backend, vector_id: &str) -> World {
    let db = TestDb::migrated(b);
    let s = db.store();
    s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let wbytes = input(&vector(vector_id));
    let w = Withdrawal::from_json(&wbytes).unwrap();
    let r0 = stored_number_at("1.0892", T0);
    let id = Receipt::from_json(&r0).unwrap().cell().id();
    assert_eq!(w.cell().id(), id, "the withdrawal vectors withdraw the stored-number cell");
    let v0 = s.put_receipt(&r0).unwrap();
    s.put_version(&id, T0, &v0).unwrap();
    World { db, s, wbytes, w, id, v0 }
}

fn put_withdrawal(x: &World) -> String {
    let h = x.s.put_receipt(&x.wbytes).unwrap();
    x.s.put_version(&x.id, TW, &h).unwrap();
    h
}

fn all_four_vectors_are_stored_through_the_put_path(b: Backend) {
    for vid in IDS {
        let x = world(b, vid);
        let h = x.s.put_receipt(&x.wbytes).unwrap();
        assert_eq!(h, x.w.hash(), "{vid}");
        // Idempotent.
        assert_eq!(x.s.put_receipt(&x.wbytes).unwrap(), h, "{vid}");
        let stored = x.s.receipt(&h).unwrap().into_owned();
        assert_eq!(stored, x.w.canonical(), "{vid}");
        assert!(Receipt::from_json(&stored).is_err(), "{vid}: a withdrawal is not a valued receipt");
        assert_eq!(Withdrawal::from_json(&stored).unwrap(), x.w, "{vid}");
        x.s.put_version(&x.id, TW, &h).unwrap();
        let v = x.s.latest(&x.id).unwrap().unwrap();
        assert_eq!((v.tx.as_str(), v.receipt_hash.as_str(), v.asof.as_str()), (TW, h.as_str(), AW), "{vid}");
        assert!(v.withdrawn, "{vid}");
    }
}

fn a_tampered_withdrawal_is_refused_not_a_panic(b: Backend) {
    let x = world(b, IDS[0]);
    let text = String::from_utf8(x.wbytes.clone()).unwrap();
    let bad = text.replace("published in error", "published by mistake");
    assert_ne!(bad, text);
    assert!(x.s.put_receipt(bad.as_bytes()).is_err());
    // Nothing of it is stored.
    assert!(x.s.receipt(x.w.hash()).is_none());
}

fn stored_asof_answers_no_value_from_the_withdrawals_asof_on(b: Backend) {
    let x = world(b, IDS[0]);
    let h = put_withdrawal(&x);
    let at = |a: &str| x.s.stored_asof(&x.id, a).unwrap();
    assert_eq!(at("2024-03-09T00:00:00.000000Z"), None);
    let v = at(T0).unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (x.v0.as_str(), false));
    // Recorded at TW, but late-signed: until its asof the value before it answers.
    let v = at(MIDDLE).unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (x.v0.as_str(), false));
    let v = at(AW).unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (h.as_str(), true));
    assert_eq!((v.tx.as_str(), v.asof.as_str(), v.cell_id.as_str()), (TW, AW, x.id.as_str()));
    let v = at(END).unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (h.as_str(), true));
    // The receipt of the answer is the withdrawal's bytes.
    assert_eq!(x.s.receipt(&v.receipt_hash).unwrap().into_owned(), x.w.canonical());
}

fn evaluate_is_none_while_withdrawn_and_a_later_value_ends_it(b: Backend) {
    let x = world(b, IDS[0]);
    put_withdrawal(&x);
    let cell = x.w.cell().clone();
    assert_eq!(x.s.evaluate(&cell, T0), Some(n("1.0892")));
    assert_eq!(x.s.evaluate(&cell, MIDDLE), Some(n("1.0892")));
    assert_eq!(x.s.evaluate(&cell, AW), None);
    assert_eq!(x.s.evaluate(&cell, AFTER), None);
    assert_eq!(x.s.evaluate(&cell, END), None);
    // A later valued version ends the withdrawal.
    let r2 = stored_number_at("1.0950", T2);
    let h2 = x.s.put_receipt(&r2).unwrap();
    x.s.put_version(&x.id, T2, &h2).unwrap();
    assert_eq!(x.s.evaluate(&cell, "2024-03-20T08:29:59.999999Z"), None);
    assert_eq!(x.s.evaluate(&cell, T2), Some(n("1.0950")));
    let v = x.s.stored_asof(&x.id, END).unwrap().unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (h2.as_str(), false));
    let v = x.s.stored_asof(&x.id, AFTER).unwrap().unwrap();
    assert!(v.withdrawn);
    // Still in the history.
    assert_eq!(x.s.versions(&x.id, FIRST, END).unwrap().len(), 3);
}

fn versions_latest_and_version_asof_list_the_withdrawal(b: Backend) {
    let x = world(b, IDS[1]);
    let h = put_withdrawal(&x);
    let all: Vec<Version> = x.s.versions(&x.id, FIRST, END).unwrap();
    assert_eq!(all.iter().map(|v| v.tx.as_str()).collect::<Vec<_>>(), [T0, TW]);
    assert_eq!(all.iter().map(|v| v.withdrawn).collect::<Vec<_>>(), [false, true]);
    assert_eq!(all[1].receipt_hash, h);
    let latest = x.s.latest(&x.id).unwrap().unwrap();
    assert_eq!((latest.receipt_hash.as_str(), latest.withdrawn), (h.as_str(), true));
    // version_asof keeps its tx-only meaning.
    let v = x.s.version_asof(&x.id, MIDDLE).unwrap().unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (h.as_str(), true));
    let v = x.s.version_asof(&x.id, T0).unwrap().unwrap();
    assert!(!v.withdrawn);
    let many = x.s.latest_many(&[x.id.as_str()]).unwrap();
    assert!(many[0].as_ref().unwrap().withdrawn);
    assert_eq!(x.s.versions_limit(&x.id, FIRST, END, 10).unwrap().len(), 2);
}

fn open_slice(x: &World) -> datumesh_core::Slice {
    slice(&format!(
        r#"{{"args":{{"base":{{"type":"string","value":"EUR"}},"date":{{"type":"date","value":"2024-03-15"}},"quote":{{"type":"string","value":"USD"}}}},"fn":"fx_rate","fn_hash":"{}","ns":"rates.example.org"}}"#,
        x.w.cell().fn_hash
    ))
}

fn slice_values_leaves_a_withdrawn_cell_out(b: Backend) {
    let x = world(b, IDS[0]);
    put_withdrawal(&x);
    let sl = open_slice(&x);
    let vs = x.s.slice_values(&sl, T0).unwrap();
    assert_eq!(vs.len(), 1);
    assert_eq!(vs[0].value, n("1.0892"));
    assert_eq!(x.s.slice_values(&sl, MIDDLE).unwrap().len(), 1);
    assert!(x.s.slice_values(&sl, AW).unwrap().is_empty());
    assert!(x.s.slice_values(&sl, END).unwrap().is_empty());
    assert!(x.s.slice_values_limit(&sl, END, 10).unwrap().is_empty());
    let r2 = stored_number_at("1.0950", T2);
    let h2 = x.s.put_receipt(&r2).unwrap();
    x.s.put_version(&x.id, T2, &h2).unwrap();
    let vs = x.s.slice_values(&sl, END).unwrap();
    assert_eq!(vs.len(), 1);
    assert_eq!((vs[0].value.clone(), vs[0].version.receipt_hash.as_str()), (n("1.0950"), h2.as_str()));
}

fn a_withdrawal_of_an_existing_cell_does_not_change_the_counts(b: Backend) {
    let x = world(b, IDS[0]);
    let fh = x.w.cell().fn_hash.clone();
    let ns = "rates.example.org";
    assert_eq!(x.s.cell_count(ns, "fx_rate", &fh).unwrap(), 1);
    assert_eq!(x.s.stored_cell_count(ns, "fx_rate", &fh).unwrap(), 1);
    let before = x.db.count("cell_counts");
    put_withdrawal(&x);
    assert_eq!(x.s.cell_count(ns, "fx_rate", &fh).unwrap(), 1);
    assert_eq!(x.s.stored_cell_count(ns, "fx_rate", &fh).unwrap(), 1);
    assert_eq!(x.db.count("cell_counts"), before);
    assert_eq!(x.db.int("SELECT CAST(COALESCE(sum(n), 0) AS BIGINT) FROM cell_counts"), 1);
}

fn put_cells_takes_a_withdrawal(b: Backend) {
    let x = world(b, IDS[2]);
    let items = [CellWrite::new(&x.wbytes, Some((x.id.as_str(), TW)))];
    let hashes = x.s.put_cells(&items).unwrap();
    assert_eq!(hashes, [x.w.hash().to_string()]);
    let v = x.s.stored_asof(&x.id, END).unwrap().unwrap();
    assert_eq!((v.receipt_hash.as_str(), v.withdrawn), (x.w.hash(), true));
    assert_eq!(x.s.evaluate(x.w.cell(), END), None);
    // The same batch again is a no-op.
    assert_eq!(x.s.put_cells(&items).unwrap(), hashes);
    assert_eq!(x.s.versions(&x.id, FIRST, END).unwrap().len(), 2);
}

fn receipts_many_and_dependents_do_not_choke_on_a_withdrawal(b: Backend) {
    let x = world(b, IDS[0]);
    let h = put_withdrawal(&x);
    let got = x.s.receipts_many(&[h.as_str(), x.v0.as_str()]).unwrap();
    assert_eq!(got[0].as_deref(), Some(x.w.canonical()));
    assert!(got[1].is_some());
    assert!(x.s.inputs(&h).unwrap().is_empty());
    assert!(x.s.dependents(&h).unwrap().is_empty());
    assert!(x.s.cell_dependents(&x.id).unwrap().is_empty());
}

backends!(
    all_four_vectors_are_stored_through_the_put_path,
    a_tampered_withdrawal_is_refused_not_a_panic,
    stored_asof_answers_no_value_from_the_withdrawals_asof_on,
    evaluate_is_none_while_withdrawn_and_a_later_value_ends_it,
    versions_latest_and_version_asof_list_the_withdrawal,
    slice_values_leaves_a_withdrawn_cell_out,
    a_withdrawal_of_an_existing_cell_does_not_change_the_counts,
    put_cells_takes_a_withdrawal,
    receipts_many_and_dependents_do_not_choke_on_a_withdrawal,
);

// ---- PostgreSQL: migration 4's NOT NULL value columns and a valueless receipt -----------------

const TRACKING: &str = "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (
    version integer PRIMARY KEY,
    name text NOT NULL,
    checksum text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
)";

/// A withdrawal planted with the 0.3.1 insert (migrations 1-3 only) does not stop migrating
/// to the latest schema (the backfill of `receipts.value_type`/`value` is not defeated by a
/// receipt that has neither), and the store reads it back.
#[test]
fn pg_a_withdrawal_row_survives_the_value_backfill_of_migration_4() {
    let db = PgDb::empty();
    let mut c = db.client();
    c.batch_execute(TRACKING).unwrap();
    for m in &datumesh_store::migrate::MIGRATIONS[..3] {
        let mut t = c.transaction().unwrap();
        t.batch_execute(m.sql).unwrap();
        t.execute(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
            &[&(m.version as i32), &m.name, &m.checksum()],
        )
        .unwrap();
        t.commit().unwrap();
    }
    let v = vector("receipt/withdrawn-primary");
    let w = Withdrawal::from_json(&input(&v)).unwrap();
    c.execute(
        "INSERT INTO receipts (hash, bytes, cell_id, tx, tx_at, asof, asof_at, fn_hash, publisher)
         VALUES ($1, $2, $3, $4, datumesh_instant($4), $5, datumesh_instant($5), $6, $7)",
        &[&w.hash(), &w.canonical(), &w.cell().id(), &w.tx(), &w.asof(), &w.cell().fn_hash, &w.publisher()],
    )
    .unwrap();
    datumesh_store::migrate::apply(&db.bare_store()).expect("migrating a database holding a withdrawal");
    let s = db.store();
    assert_eq!(s.receipt(w.hash()).unwrap().into_owned(), w.canonical());
}
