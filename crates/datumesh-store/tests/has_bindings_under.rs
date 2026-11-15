//! `has_bindings_under` (CONTRACT-1.8b S2): true when any name binding or class binding has
//! a name starting with `namespace + "/"`.

#[macro_use]
mod common;

use common::*;

const T1: &str = "2024-03-15T16:00:00.000000Z";

fn a_name_binding_makes_its_namespace_true(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.store();
    // An empty store.
    assert!(!s.has_bindings_under("a.example/ns").unwrap());
    let h = s.put_descriptor(&fx_rate_descriptor()).unwrap();
    s.bind_name("a.example/ns/f", &h, T1).unwrap();
    assert!(s.has_bindings_under("a.example/ns").unwrap());
    // "a.example/n" is a prefix of the name, but not a namespace of it.
    assert!(!s.has_bindings_under("a.example/n").unwrap());
}

fn a_class_binding_alone_makes_its_namespace_true(b: Backend) {
    let db = TestDb::migrated(b);
    let s = db.bare_store();
    assert!(!s.has_bindings_under("b.example/k").unwrap());
    let c = test_class();
    let hash = put_test_class(&s, &c);
    s.bind_class("b.example/k/c", &hash, T1).unwrap();
    assert!(s.has_bindings_under("b.example/k").unwrap());
    assert!(!s.has_bindings_under("a.example/ns").unwrap());
}

backends!(a_name_binding_makes_its_namespace_true, a_class_binding_alone_makes_its_namespace_true);
