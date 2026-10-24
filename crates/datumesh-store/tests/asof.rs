//! As-of reads: v1 at tx1, v2 at tx2. Before tx1 nothing; from tx1 (inclusive) v1; from
//! tx2 (inclusive) v2. `versions` lists the history in tx order between two instants, both
//! inclusive; `latest` is v2. `evaluate` of the stored function follows the same rule when
//! asof = tx; with a late-signed receipt it answers with both times (`stored_asof`), while
//! `version_asof` keeps looking at tx only.

mod common;

use common::*;
use datumesh_core::{Receipt, Resolver, Scalar, Type};
use datumesh_store::{Store, Version};

const TX1: &str = "2024-03-15T16:00:00.000000Z";
const TX2: &str = "2024-03-18T10:00:00.000000Z";

#[test]
fn as_of_before_between_after_and_at() {
    let db = TestDb::migrated();
    let s = db.store();
    s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let r1 = stored_number_at("1.0892", TX1);
    let r2 = stored_number_at("1.0893", TX2);
    let h1 = s.put_receipt(&r1).unwrap();
    let h2 = s.put_receipt(&r2).unwrap();
    let cell = Receipt::from_json(&r1).unwrap().cell().clone();
    let id = cell.id();
    assert_eq!(s.latest(&id).unwrap(), None);
    s.put_version(&id, TX1, &h1).unwrap();
    s.put_version(&id, TX2, &h2).unwrap();

    let at = |asof: &str| s.version_asof(&id, asof).unwrap().map(|v| v.receipt_hash);
    assert_eq!(at("0001-01-01T00:00:00.000000Z"), None);
    assert_eq!(at("2024-03-15T15:59:59.999999Z"), None);
    assert_eq!(at(TX1).as_ref(), Some(&h1));
    assert_eq!(at("2024-03-17T00:00:00.000000Z").as_ref(), Some(&h1));
    assert_eq!(at("2024-03-18T09:59:59.999999Z").as_ref(), Some(&h1));
    assert_eq!(at(TX2).as_ref(), Some(&h2));
    assert_eq!(at("9999-12-31T23:59:59.999999Z").as_ref(), Some(&h2));

    let v1 = s.version_asof(&id, TX1).unwrap().unwrap();
    assert_eq!((v1.cell_id.as_str(), v1.tx.as_str(), v1.receipt_hash.as_str()), (id.as_str(), TX1, h1.as_str()));
    // The version's asof is its receipt's (here equal to its tx).
    assert_eq!(v1.asof, TX1);

    let all: Vec<Version> = s.versions(&id, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z").unwrap();
    assert_eq!(all.iter().map(|v| v.tx.as_str()).collect::<Vec<_>>(), [TX1, TX2]);
    assert_eq!(all.iter().map(|v| v.receipt_hash.as_str()).collect::<Vec<_>>(), [h1.as_str(), h2.as_str()]);
    // Both bounds inclusive.
    assert_eq!(s.versions(&id, TX1, TX2).unwrap().len(), 2);
    assert_eq!(s.versions(&id, TX1, TX1).unwrap().len(), 1);
    assert_eq!(s.versions(&id, "2024-03-15T16:00:00.000001Z", TX2).unwrap().len(), 1);
    assert_eq!(s.versions(&id, TX2, TX1).unwrap().len(), 0);
    assert_eq!(all.iter().map(|v| v.asof.as_str()).collect::<Vec<_>>(), [TX1, TX2]);
    let latest = s.latest(&id).unwrap().unwrap();
    assert_eq!((latest.tx.as_str(), latest.asof.as_str(), latest.receipt_hash.as_str()), (TX2, TX2, h2.as_str()));

    // evaluate: the stored value as of asof.
    let n = |x: &str| Scalar::from_lexical(Type::Number, x).unwrap();
    assert_eq!(s.evaluate(&cell, "2024-03-15T15:59:59.999999Z"), None);
    assert_eq!(s.evaluate(&cell, TX1), Some(n("1.0892")));
    assert_eq!(s.evaluate(&cell, "2024-03-17T00:00:00.000000Z"), Some(n("1.0892")));
    assert_eq!(s.evaluate(&cell, TX2), Some(n("1.0893")));

    // Another cell has no versions.
    let other = "0".repeat(64);
    assert_eq!(s.latest(&other).unwrap(), None);
    assert_eq!(s.version_asof(&other, TX2).unwrap(), None);
    assert!(s.versions(&other, TX1, TX2).unwrap().is_empty());
}

#[test]
fn evaluate_is_none_for_functions_that_are_not_stored() {
    // computed-with-lineage is a convert (expr) cell: even with a version recorded for it,
    // the store does not evaluate it.
    let db = TestDb::migrated();
    let s = db.store();
    let v = vector("receipt/computed-with-lineage");
    let rc = v.get("recompute").unwrap();
    s.put_descriptor(&rc.get("descriptor").unwrap().to_jcs()).unwrap();
    let bytes = input(&v);
    let h = s.put_receipt(&bytes).unwrap();
    let r = Receipt::from_json(&bytes).unwrap();
    s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
    assert_eq!(s.evaluate(r.cell(), r.asof()), None);
    // And without its descriptor, nothing is evaluated either.
    let db2 = TestDb::migrated();
    let s2 = db2.store();
    let sn = vector("receipt/stored-number");
    let b = input(&sn);
    let h = s2.put_receipt(&b).unwrap();
    let r = Receipt::from_json(&b).unwrap();
    s2.put_version(&r.cell().id(), r.tx(), &h).unwrap();
    assert_eq!(s2.evaluate(r.cell(), r.asof()), None);
}

/// `evaluate` answers as `stored_asof` does (the kit's `AnswerRule::TxAndAsof`): a version
/// answers as of A only when its tx and its receipt's asof are both at or before A. The
/// vectors' stored-number receipt (tx 2024-03-15T16:00, asof 2024-03-16T09:00) gives no value
/// from its tx until its asof; a late-signed second version (tx 2024-03-18T10:00, asof
/// 2024-03-19T12:00) leaves the first answering until its asof. `version_asof` is unchanged.
#[test]
fn evaluate_answers_with_both_times() {
    let db = TestDb::migrated();
    let s = db.store();
    s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let v = vector("receipt/stored-number");
    let b1 = input(&v);
    let r1 = Receipt::from_json(&b1).unwrap();
    let (tx1, asof1) = ("2024-03-15T16:00:00.000000Z", "2024-03-16T09:00:00.000000Z");
    assert_eq!((r1.tx(), r1.asof()), (tx1, asof1));
    let (tx2, asof2) = ("2024-03-18T10:00:00.000000Z", "2024-03-19T12:00:00.000000Z");
    let b2 = stored_number_with("1.0893", tx2, asof2);
    let h1 = s.put_receipt(&b1).unwrap();
    let h2 = s.put_receipt(&b2).unwrap();
    let cell = r1.cell().clone();
    let id = cell.id();
    s.put_version(&id, tx1, &h1).unwrap();
    s.put_version(&id, tx2, &h2).unwrap();

    let v1 = Some(r1.value().clone());
    let v2 = Some(Scalar::from_lexical(Type::Number, "1.0893").unwrap());
    let cases = [
        ("2024-03-15T15:59:59.999999Z", None, None),
        // From v1's tx until (not including) its asof: no value; version_asof says v1.
        (tx1, None, Some(&h1)),
        ("2024-03-16T00:00:00.000000Z", None, Some(&h1)),
        ("2024-03-16T08:59:59.999999Z", None, Some(&h1)),
        (asof1, v1.clone(), Some(&h1)),
        ("2024-03-17T00:00:00.000000Z", v1.clone(), Some(&h1)),
        // v2 recorded but not yet known: v1 still answers; version_asof says v2.
        (tx2, v1.clone(), Some(&h2)),
        ("2024-03-19T11:59:59.999999Z", v1.clone(), Some(&h2)),
        (asof2, v2.clone(), Some(&h2)),
        ("9999-12-31T23:59:59.999999Z", v2.clone(), Some(&h2)),
    ];
    for (a, value, by_tx) in cases {
        assert_eq!(s.evaluate(&cell, a), value, "evaluate as of {a}");
        assert_eq!(s.version_asof(&id, a).unwrap().map(|v| v.receipt_hash).as_ref(), by_tx, "version_asof as of {a}");
        // evaluate is the value of stored_asof's receipt.
        let by_both = s.stored_asof(&id, a).unwrap().map(|v| v.receipt_hash);
        let want = by_both.map(|h| if h == h1 { v1.clone().unwrap() } else { v2.clone().unwrap() });
        assert_eq!(s.evaluate(&cell, a), want, "stored_asof as of {a}");
    }
}
