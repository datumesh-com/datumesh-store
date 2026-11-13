//! Namespace checks (domain validation): the store records the outcome of each check of a
//! host's key document. `record_namespace_failure` counts consecutive failures and, at the
//! given threshold, suspends the namespace (`suspended_at`, the single source of truth);
//! `record_namespace_success` clears the count and the suspension. While a namespace is
//! suspended the store refuses new writes under it (`put_receipt` of a receipt signed by the
//! host, `put_version` of such a receipt, `bind_name` of a name under the host) with
//! `Error::NamespaceSuspended`; reads, and putting a receipt that is already stored (it
//! verified when it was stored), go on. `put_namespace` replaces the document (a rotation)
//! and neither suspends nor reinstates. Both backends.

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Receipt, Resolver};
use datumesh_store::Error;

const HOST: &str = "rates.example.org";
const T1: &str = "2024-03-16T09:00:00.000000Z";
const T2: &str = "2024-03-23T09:00:00.000000Z";
const T3: &str = "2024-03-30T09:00:00.000000Z";
const T4: &str = "2024-04-06T09:00:00.000000Z";

fn suspended(e: Error) -> (String, String) {
    match e {
        Error::NamespaceSuspended { host, suspended_at } => (host, suspended_at),
        e => panic!("not NamespaceSuspended: {e:?}"),
    }
}

fn a_new_namespace_has_no_failures_and_no_check(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let doc = key_doc(&vector("receipt/stored-number"), HOST);
    s.put_namespace(HOST, &doc, T1).unwrap();
    let ns = s.namespace(HOST).unwrap().unwrap();
    assert_eq!(ns.consecutive_failures, 0);
    assert_eq!(ns.last_checked_at, None);
    assert_eq!(ns.suspended_at, None);
}

fn the_second_consecutive_failure_suspends(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_namespace(HOST, &key_doc(&vector("receipt/stored-number"), HOST), T1).unwrap();
    let ns = s.record_namespace_failure(HOST, T2, 2).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref(), ns.last_checked_at.as_deref()), (1, None, Some(T2)));
    let ns = s.record_namespace_failure(HOST, T3, 2).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref(), ns.last_checked_at.as_deref()), (2, Some(T3), Some(T3)));
    // What the store reads back is what it returned.
    assert_eq!(s.namespace(HOST).unwrap().unwrap(), ns);
    // A further failure counts, and keeps the time the suspension began.
    let ns = s.record_namespace_failure(HOST, T4, 2).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref()), (3, Some(T3)));
}

fn a_threshold_of_zero_is_one(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_namespace(HOST, &key_doc(&vector("receipt/stored-number"), HOST), T1).unwrap();
    let ns = s.record_namespace_failure(HOST, T2, 0).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref()), (1, Some(T2)));
}

fn a_success_reinstates_and_resets_the_count(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_namespace(HOST, &key_doc(&vector("receipt/stored-number"), HOST), T1).unwrap();
    s.record_namespace_failure(HOST, T2, 2).unwrap();
    // A success between failures resets the count: failures must be consecutive.
    let ns = s.record_namespace_success(HOST, T3).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref(), ns.last_checked_at.as_deref()), (0, None, Some(T3)));
    let ns = s.record_namespace_failure(HOST, T4, 2).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref()), (1, None));
    // Suspend, then reinstate.
    let t5 = "2024-04-13T09:00:00.000000Z";
    let t6 = "2024-04-20T09:00:00.000000Z";
    assert!(s.record_namespace_failure(HOST, t5, 2).unwrap().unwrap().suspended_at.is_some());
    let ns = s.record_namespace_success(HOST, t6).unwrap().unwrap();
    assert_eq!((ns.consecutive_failures, ns.suspended_at.as_deref(), ns.last_checked_at.as_deref()), (0, None, Some(t6)));
}

fn put_namespace_replaces_the_document_and_leaves_the_checks_alone(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let doc = key_doc(&vector("receipt/stored-number"), HOST);
    s.put_namespace(HOST, &doc, T1).unwrap();
    s.record_namespace_failure(HOST, T2, 1).unwrap();
    s.put_namespace(HOST, &doc, T3).unwrap();
    let ns = s.namespace(HOST).unwrap().unwrap();
    assert_eq!(ns.validated_at, T3);
    assert_eq!(ns.suspended_at.as_deref(), Some(T2));
    assert_eq!(ns.consecutive_failures, 1);
    assert_eq!(ns.last_checked_at.as_deref(), Some(T2));
}

fn an_unknown_host_has_nothing_to_record(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    assert_eq!(s.record_namespace_failure("nowhere.example.org", T2, 2).unwrap(), None);
    assert_eq!(s.record_namespace_success("nowhere.example.org", T2).unwrap(), None);
    assert_eq!(s.namespace("nowhere.example.org").unwrap(), None);
}

fn a_check_is_recorded_forward_in_time_only(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    s.put_namespace(HOST, &key_doc(&vector("receipt/stored-number"), HOST), T1).unwrap();
    s.record_namespace_failure(HOST, T3, 5).unwrap();
    assert!(matches!(s.record_namespace_failure(HOST, T2, 5), Err(Error::NotLater { what: "namespace", .. })));
    assert!(matches!(s.record_namespace_success(HOST, T2), Err(Error::NotLater { what: "namespace", .. })));
    assert_eq!(s.namespace(HOST).unwrap().unwrap().consecutive_failures, 1);
    // Malformed arguments: the core's codes.
    assert_eq!(
        s.record_namespace_failure(HOST, "yesterday", 2).unwrap_err().code(),
        Some(datumesh_core::ErrorCode::CanonInstant)
    );
    assert_eq!(
        s.record_namespace_success("Not A Host", T4).unwrap_err().code(),
        Some(datumesh_core::ErrorCode::CanonDnsName)
    );
}

fn namespaces_lists_every_host_in_order(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    assert!(s.namespaces().unwrap().is_empty());
    let v = vector("receipt/stored-number");
    let lineage = vector("receipt/computed-with-lineage");
    s.put_namespace(HOST, &key_doc(&v, HOST), T1).unwrap();
    s.put_namespace("calc.example.net", &key_doc(&lineage, "calc.example.net"), T1).unwrap();
    s.record_namespace_failure(HOST, T2, 1).unwrap();
    let all = s.namespaces().unwrap();
    assert_eq!(all.iter().map(|n| n.host.as_str()).collect::<Vec<_>>(), ["calc.example.net", HOST]);
    assert_eq!(all[0].suspended_at, None);
    assert_eq!(all[1].suspended_at.as_deref(), Some(T2));
    assert_eq!(all[1], s.namespace(HOST).unwrap().unwrap());
}

fn a_suspended_namespace_refuses_new_writes_but_not_reads(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v = vector("receipt/stored-number");
    put_vector_keys(&s, &v);
    let bytes = input(&v);
    let r = Receipt::from_json(&bytes).unwrap();
    let fn_hash = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    let h = s.put_receipt(&bytes).unwrap();
    s.put_version(&r.cell().id(), r.tx(), &h).unwrap();
    let later = stored_number_at("1.2345", "2024-03-20T10:00:00.000000Z");
    let later_r = Receipt::from_json(&later).unwrap();

    s.record_namespace_failure(HOST, T2, 1).unwrap();

    // A receipt signed by the host, not stored yet: refused.
    assert_eq!(suspended(s.put_receipt(&later).unwrap_err()), (HOST.to_string(), T2.to_string()));
    assert_eq!(s.receipt(later_r.hash()), None);
    // A name under the host.
    assert_eq!(
        suspended(s.bind_name(&format!("{HOST}/fx_rate"), &fn_hash, r.tx()).unwrap_err()),
        (HOST.to_string(), T2.to_string())
    );
    assert_eq!(s.resolve_name(&format!("{HOST}/fx_rate"), "9999-12-31T23:59:59.999999Z").unwrap(), None);
    // A version of a stored receipt signed by the host: put the receipt first, then suspend.
    let db2 = TestDb::migrated(b);
    let s2 = db2.bare_store();
    put_vector_keys(&s2, &v);
    s2.put_descriptor(&fx_rate_descriptor()).unwrap();
    let h2 = s2.put_receipt(&bytes).unwrap();
    s2.record_namespace_failure(HOST, T2, 1).unwrap();
    assert_eq!(
        suspended(s2.put_version(&r.cell().id(), r.tx(), &h2).unwrap_err()),
        (HOST.to_string(), T2.to_string())
    );
    assert_eq!(s2.latest(&r.cell().id()).unwrap(), None);

    // Reads and verification go on: the receipt, the version, the answer, the document.
    assert!(s.receipt(&h).is_some());
    assert_eq!(s.latest(&r.cell().id()).unwrap().unwrap().receipt_hash, h);
    assert!(s.namespace(HOST).unwrap().is_some());
    // Putting the very receipt again stays idempotent: it verified when it was stored.
    assert_eq!(s.put_receipt(&bytes).unwrap(), h);

    // Reinstated: the writes go through.
    s.record_namespace_success(HOST, T3).unwrap();
    assert_eq!(s.put_receipt(&later).unwrap(), later_r.hash());
    s.put_version(&later_r.cell().id(), later_r.tx(), later_r.hash()).unwrap();
    s.bind_name(&format!("{HOST}/fx_rate"), &fn_hash, r.tx()).unwrap();
}

fn another_host_is_not_affected(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    let v = vector("receipt/stored-number");
    let lineage = vector("receipt/computed-with-lineage");
    put_vector_keys(&s, &v);
    put_vector_keys(&s, &lineage);
    s.record_namespace_failure(HOST, T2, 1).unwrap();
    // The lineage vector's receipt is signed by another host.
    let other = Receipt::from_json(&input(&lineage)).unwrap();
    assert_ne!(other.signer_host(), HOST);
    assert_eq!(s.put_receipt(&input(&lineage)).unwrap(), other.hash());
}

backends!(
    a_new_namespace_has_no_failures_and_no_check,
    the_second_consecutive_failure_suspends,
    a_threshold_of_zero_is_one,
    a_success_reinstates_and_resets_the_count,
    put_namespace_replaces_the_document_and_leaves_the_checks_alone,
    an_unknown_host_has_nothing_to_record,
    a_check_is_recorded_forward_in_time_only,
    namespaces_lists_every_host_in_order,
    a_suspended_namespace_refuses_new_writes_but_not_reads,
    another_host_is_not_affected,
);
