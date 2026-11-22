//! The public API names no type of the PostgreSQL driver or of TLS crates outside
//! `datumesh_store::pg_unstable`: checked on `public-api.txt` (`cargo public-api -ss`, the
//! crate has no features, so default and all features are the same), which CI regenerates
//! and compares (tools/public_api.sh).

/// Crates whose names must not appear in the API outside `pg_unstable`.
const FORBIDDEN: &[&str] = &[
    "postgres",
    "tokio_postgres",
    "postgres_types",
    "postgres_protocol",
    "rustls",
    "tokio_rustls",
    "tokio_postgres_rustls",
    "rustls_pki_types",
    "pki_types",
    "webpki",
    "webpki_roots",
    "ring",
    "rusqlite",
    "libsqlite3_sys",
];

fn api() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../public-api.txt");
    std::fs::read_to_string(path).expect("public-api.txt")
}

/// The identifiers of a line.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty())
}

#[test]
fn no_driver_or_tls_types_outside_pg_unstable() {
    let api = api();
    let leaks: Vec<&str> = api
        .lines()
        .filter(|l| !l.contains("datumesh_store::pg_unstable"))
        .filter(|l| words(l).any(|w| FORBIDDEN.contains(&w)))
        .collect();
    assert!(leaks.is_empty(), "outside pg_unstable:\n{}", leaks.join("\n"));
}

#[test]
fn the_api_has_pg_unstable_and_the_replacements() {
    let api = api();
    let has = |item: &str| api.lines().any(|l| l == item);
    for item in [
        "pub mod datumesh_store::pg_unstable",
        "pub use datumesh_store::pg_unstable::postgres",
        "pub struct datumesh_store::DatabaseError",
        "pub fn datumesh_store::DatabaseError::code(&self) -> core::option::Option<&str>",
        "pub fn datumesh_store::DatabaseError::message(&self) -> &str",
        "pub datumesh_store::Error::Database(datumesh_store::DatabaseError)",
        "pub fn datumesh_store::migrate::apply(&datumesh_store::PgStore) -> datumesh_store::Result<alloc::vec::Vec<u32>>",
        "pub fn datumesh_store::migrate::catalog(&datumesh_store::PgStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>",
        "pub fn datumesh_store::migrate::check(&datumesh_store::PgStore, &datumesh_store::PgStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>",
        "pub fn datumesh_store::PgStore::connect(&str) -> datumesh_store::Result<datumesh_store::PgStore>",
        "pub fn datumesh_store::PgStore::connect_with(&str, datumesh_store::PoolOptions) -> datumesh_store::Result<datumesh_store::PgStore>",
        "pub fn datumesh_store::PoolOptions::tls_root_certificates_pem(self, &[u8]) -> datumesh_store::Result<datumesh_store::PoolOptions>",
        "pub fn datumesh_store::PoolOptions::tls_webpki_roots(self, bool) -> datumesh_store::PoolOptions",
    ] {
        assert!(has(item), "missing: {item}");
    }
    // 0.2.0: computed versions and answering as of A, on the trait and on PgStore.
    for owner in ["Store", "PgStore"] {
        for item in [
            format!("pub fn datumesh_store::{owner}::put_computed(&self, &str, &str, &str) -> datumesh_store::Result<()>"),
            format!("pub fn datumesh_store::{owner}::computed_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>"),
            format!("pub fn datumesh_store::{owner}::stored_asof(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Version>>"),
            format!("pub fn datumesh_store::{owner}::cell_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>"),
        ] {
            assert!(has(&item), "missing: {item}");
        }
        for prefix in
            ["computed_asof_with_receipt(&self, &str, &str) -> ", "stored_asof_with_receipt(&self, &str, &str) -> "]
        {
            let p = format!("pub fn datumesh_store::{owner}::{prefix}");
            assert!(api.lines().any(|l| l.starts_with(&p)), "missing: {p}");
        }
    }
    assert!(has("pub datumesh_store::Version::asof: alloc::string::String"));
    // Nothing of 0.1.2 removed: the Version fields stay.
    for f in ["cell_id", "receipt_hash", "tx"] {
        assert!(has(&format!("pub datumesh_store::Version::{f}: alloc::string::String")));
    }
    // 0.3.0: the binding answering as of A, with when it began.
    for item in [
        "#[non_exhaustive] pub struct datumesh_store::Binding",
        "pub datumesh_store::Binding::fn_hash: alloc::string::String",
        "pub datumesh_store::Binding::tx: alloc::string::String",
        "pub fn datumesh_store::Store::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>",
        "pub fn datumesh_store::PgStore::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>",
    ] {
        assert!(has(item), "missing: {item}");
    }

    // 0.3.1: slices, manifests, version ranges, slice-entry lineage.
    for owner in ["Store", "PgStore"] {
        for item in [
            format!("pub fn datumesh_store::{owner}::put_manifest(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>"),
            format!("pub fn datumesh_store::{owner}::member_manifests(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>"),
            format!("pub fn datumesh_store::{owner}::slice_entries(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>"),
            format!("pub fn datumesh_store::{owner}::slice_dependents(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>"),
            format!("pub fn datumesh_store::{owner}::versions_between(&self, &str, &str, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>"),
        ] {
            assert!(has(&item), "missing: {item}");
        }
        let p = format!("pub fn datumesh_store::{owner}::slice_members(&self, &datumesh_core::");
        assert!(
            api.lines().any(|l| l.starts_with(&p)
                && l.ends_with("Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceMember>>")),
            "missing: {p}"
        );
    }
    for item in [
        "#[non_exhaustive] pub struct datumesh_store::SliceMember",
        "pub datumesh_store::SliceMember::version: datumesh_store::Version",
        "pub datumesh_store::SliceMember::receipt: alloc::vec::Vec<u8>",
        "pub fn datumesh_store::PgStore::manifest(&self, &str) -> core::option::Option<alloc::borrow::Cow<'_, [u8]>>",
    ] {
        assert!(has(item), "missing: {item}");
    }

    // 0.3.2: member values without receipt bytes, and whether a slice changes after A.
    for owner in ["Store", "PgStore"] {
        for (m, ret) in [
            ("slice_values", "datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>"),
            ("slice_changed_after", "datumesh_store::Result<bool>"),
        ] {
            let item =
                format!("pub fn datumesh_store::{owner}::{m}(&self, &datumesh_core::slice::Slice, &str) -> {ret}");
            assert!(has(&item), "missing: {item}");
        }
    }
    for item in [
        "#[non_exhaustive] pub struct datumesh_store::SliceValue",
        "pub datumesh_store::SliceValue::version: datumesh_store::Version",
        "pub datumesh_store::SliceValue::value: datumesh_core::model::Scalar",
        "pub datumesh_store::SliceValue::cell: datumesh_core::model::Cell",
    ] {
        assert!(has(item), "missing: {item}");
    }

    // 0.4.0: the SQLite backend, every Store method on it, its options, its migrations.
    for item in [
        "pub struct datumesh_store::SqliteStore",
        "pub struct datumesh_store::SqliteOptions",
        "pub fn datumesh_store::SqliteStore::open(impl core::convert::AsRef<std::path::Path>) -> datumesh_store::Result<datumesh_store::SqliteStore>",
        "pub fn datumesh_store::SqliteStore::open_with(impl core::convert::AsRef<std::path::Path>, datumesh_store::SqliteOptions) -> datumesh_store::Result<datumesh_store::SqliteStore>",
        "pub fn datumesh_store::SqliteStore::connect(&str) -> datumesh_store::Result<datumesh_store::SqliteStore>",
        "pub fn datumesh_store::SqliteStore::check_statements(&self) -> datumesh_store::Result<usize>",
        "pub fn datumesh_store::SqliteStore::open_existing(impl core::convert::AsRef<std::path::Path>) -> datumesh_store::Result<datumesh_store::SqliteStore>",
        "pub fn datumesh_store::migrate::sqlite::check_file(impl core::convert::AsRef<std::path::Path>) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>",
        "pub fn datumesh_store::SqliteOptions::busy_timeout(self, core::time::Duration) -> datumesh_store::SqliteOptions",
        "pub fn datumesh_store::SqliteOptions::durable(self, bool) -> datumesh_store::SqliteOptions",
        "pub fn datumesh_store::SqliteOptions::max_size(self, usize) -> datumesh_store::SqliteOptions",
        "pub mod datumesh_store::migrate::sqlite",
        "pub const datumesh_store::migrate::sqlite::MIGRATIONS: &[datumesh_store::migrate::Migration]",
        "pub fn datumesh_store::migrate::sqlite::apply(&datumesh_store::SqliteStore) -> datumesh_store::Result<alloc::vec::Vec<u32>>",
        "pub fn datumesh_store::migrate::sqlite::catalog(&datumesh_store::SqliteStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>",
        "pub fn datumesh_store::migrate::sqlite::check(&datumesh_store::SqliteStore) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>",
    ] {
        assert!(has(item), "missing: {item}");
    }
    let store_methods: Vec<&str> = api
        .lines()
        .filter_map(|l| l.strip_prefix("pub fn datumesh_store::Store::"))
        .filter_map(|l| l.split('(').next())
        .collect();
    assert!(store_methods.len() >= 29, "{store_methods:?}");
    for m in store_methods {
        for owner in ["PgStore", "SqliteStore"] {
            let p = format!("pub fn datumesh_store::{owner}::{m}(");
            assert!(api.lines().any(|l| l.starts_with(&p)), "missing: {p}");
        }
    }

    // Sealed: the private supertrait (named only in the listing; no crate can implement it).
    assert!(has("pub trait datumesh_store::Store: datumesh_core::verify::Resolver + datumesh_store::sealed::Sealed"));

    // Moved, not kept at the root.
    assert!(!api.contains("datumesh_store::PgStore::with_connector"));
    assert!(api.lines().any(|l| l.starts_with("pub fn datumesh_store::pg_unstable::with_connector<F>(")));
    assert!(!api.lines().any(|l| l == "pub use datumesh_store::postgres"));
}

/// 0.4.1: classes, additively (`public-api.txt` is regenerated with the implementation).
#[test]
fn the_api_has_classes_since_0_4_1() {
    let api = api();
    let has = |item: &str| api.lines().any(|l| l == item);
    for owner in ["Store", "PgStore", "SqliteStore"] {
        for item in [
            format!("pub fn datumesh_store::{owner}::put_class(&self, &[u8]) -> datumesh_store::Result<alloc::string::String>"),
            format!("pub fn datumesh_store::{owner}::class(&self, &str) -> datumesh_store::Result<core::option::Option<alloc::vec::Vec<u8>>>"),
            format!("pub fn datumesh_store::{owner}::bind_class(&self, &str, &str, &str) -> datumesh_store::Result<()>"),
            format!("pub fn datumesh_store::{owner}::resolve_class(&self, &str, &str) -> datumesh_store::Result<core::option::Option<alloc::string::String>>"),
            format!("pub fn datumesh_store::{owner}::resolve_class_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::ClassBinding>>"),
        ] {
            assert!(has(&item), "missing: {item}");
        }
    }
    for item in [
        "#[non_exhaustive] pub struct datumesh_store::ClassBinding",
        "pub datumesh_store::ClassBinding::class_hash: alloc::string::String",
        "pub datumesh_store::ClassBinding::tx: alloc::string::String",
        "pub datumesh_store::Error::NameTaken",
        "pub datumesh_store::Error::NameTaken::bound_as: &'static str",
        "pub datumesh_store::Error::NameTaken::name: alloc::string::String",
    ] {
        assert!(has(item), "missing: {item}");
    }
    // Additions only: what 0.4.0 and the namespace checks had is still there.
    for item in [
        "pub fn datumesh_store::Store::bind_name(&self, &str, &str, &str) -> datumesh_store::Result<()>",
        "pub fn datumesh_store::Store::resolve_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::Binding>>",
        "pub datumesh_store::Error::NamespaceSuspended",
        "pub datumesh_store::Error::NotFound::what: &'static str",
    ] {
        assert!(has(item), "missing: {item}");
    }
}

/// 0.4.2: `computed_cells` and `has_bindings_under`, additively (`public-api.txt` is
/// regenerated with the implementation).
#[test]
fn the_api_has_computed_cells_and_has_bindings_under_since_0_4_2() {
    let api = api();
    let has = |item: &str| api.lines().any(|l| l == item);
    for owner in ["Store", "PgStore", "SqliteStore"] {
        for item in [
            format!("pub fn datumesh_store::{owner}::computed_cells(&self, &datumesh_core::slice::Slice, &str, usize) -> datumesh_store::Result<alloc::vec::Vec<datumesh_core::model::Cell>>"),
            format!("pub fn datumesh_store::{owner}::has_bindings_under(&self, &str) -> datumesh_store::Result<bool>"),
        ] {
            assert!(has(&item), "missing: {item}");
        }
    }
    // Additions only: the 0.3.2 slice methods are still there.
    assert!(has("pub fn datumesh_store::Store::slice_values(&self, &datumesh_core::slice::Slice, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::SliceValue>>"));
}

/// 0.5.3 / 0.6.0 (CONTRACT-classes-round C2 and K9): catalogue facets and `extends`, additively
/// (`public-api.txt` is regenerated with the implementation). `CatalogEntry` itself is unchanged.
#[test]
fn the_api_has_catalogue_facets_and_extends() {
    let api = api();
    let has = |item: &str| api.lines().any(|l| l == item);
    for item in [
        "pub type datumesh_store::Facets = alloc::collections::btree::map::BTreeMap<alloc::string::String, alloc::vec::Vec<alloc::string::String>>",
        "#[non_exhaustive] pub struct datumesh_store::CatalogEntryExt",
        "pub datumesh_store::CatalogEntryExt::entry: datumesh_store::CatalogEntry",
        "pub datumesh_store::CatalogEntryExt::extends: core::option::Option<alloc::string::String>",
        "pub datumesh_store::CatalogEntryExt::facets: datumesh_store::Facets",
        "pub fn datumesh_store::CatalogEntryExt::new(datumesh_store::CatalogEntry) -> datumesh_store::CatalogEntryExt",
        "#[non_exhaustive] pub struct datumesh_store::FacetedEntry",
        "pub datumesh_store::FacetedEntry::facets: datumesh_store::Facets",
        "pub datumesh_store::FacetedEntry::first_bound: core::option::Option<alloc::string::String>",
        "pub datumesh_store::FacetedEntry::hash: core::option::Option<alloc::string::String>",
        "pub datumesh_store::FacetedEntry::kind: datumesh_store::CatalogKind",
        "pub datumesh_store::FacetedEntry::name: alloc::string::String",
    ] {
        assert!(has(item), "missing: {item}");
    }
    for owner in ["Store", "PgStore", "SqliteStore"] {
        for item in [
            format!("pub fn datumesh_store::{owner}::upsert_catalog_ext(&self, &[datumesh_store::CatalogEntryExt]) -> datumesh_store::Result<alloc::vec::Vec<i64>>"),
            format!("pub fn datumesh_store::{owner}::catalog_entry_ext(&self, datumesh_store::CatalogKind, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::CatalogEntryExt>>"),
            format!("pub fn datumesh_store::{owner}::catalog_entries_with_facets(&self) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::FacetedEntry>>"),
            format!("pub fn datumesh_store::{owner}::extensions_of(&self, &str) -> datumesh_store::Result<alloc::vec::Vec<datumesh_store::CatalogEntryExt>>"),
        ] {
            assert!(has(&item), "missing: {item}");
        }
    }
    // Additions only: CatalogEntry keeps exactly its nine public fields and is not non_exhaustive.
    assert!(has("pub struct datumesh_store::CatalogEntry"));
    assert!(has("pub datumesh_store::CatalogEntry::signed_by: core::option::Option<alloc::string::String>"));
    assert!(!api.lines().any(|l| l.starts_with("pub datumesh_store::CatalogEntry::facets")));
}

/// CONTRACT-classes-round (the K5 follow-up): `class_names_bound_to`, additively (`public-api.txt` is
/// regenerated with the implementation). A new `Store` method with a default body.
#[test]
fn the_api_has_class_names_bound_to() {
    let api = api();
    let has = |item: &str| api.lines().any(|l| l == item);
    for owner in ["Store", "PgStore", "SqliteStore"] {
        let item = format!("pub fn datumesh_store::{owner}::class_names_bound_to(&self, &str, &str) -> datumesh_store::Result<alloc::vec::Vec<alloc::string::String>>");
        assert!(has(&item), "missing: {item}");
    }
    // Additions only: the class accessors it joins are still there.
    assert!(has("pub fn datumesh_store::Store::resolve_class_binding(&self, &str, &str) -> datumesh_store::Result<core::option::Option<datumesh_store::ClassBinding>>"));
}
