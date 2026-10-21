//! As-of reads: v1 at tx1, v2 at tx2. Before tx1 nothing; from tx1 (inclusive) v1; from
//! tx2 (inclusive) v2. `versions` lists the history in tx order between two instants, both
//! inclusive; `latest` is v2. `evaluate` of the stored function follows the same rule.

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

    let all: Vec<Version> = s.versions(&id, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z").unwrap();
    assert_eq!(all.iter().map(|v| v.tx.as_str()).collect::<Vec<_>>(), [TX1, TX2]);
    assert_eq!(all.iter().map(|v| v.receipt_hash.as_str()).collect::<Vec<_>>(), [h1.as_str(), h2.as_str()]);
    // Both bounds inclusive.
    assert_eq!(s.versions(&id, TX1, TX2).unwrap().len(), 2);
    assert_eq!(s.versions(&id, TX1, TX1).unwrap().len(), 1);
    assert_eq!(s.versions(&id, "2024-03-15T16:00:00.000001Z", TX2).unwrap().len(), 1);
    assert_eq!(s.versions(&id, TX2, TX1).unwrap().len(), 0);
    let latest = s.latest(&id).unwrap().unwrap();
    assert_eq!((latest.tx.as_str(), latest.receipt_hash.as_str()), (TX2, h2.as_str()));

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
