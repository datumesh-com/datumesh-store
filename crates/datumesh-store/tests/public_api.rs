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
    // Moved, not kept at the root.
    assert!(!api.contains("datumesh_store::PgStore::with_connector"));
    assert!(api.lines().any(|l| l.starts_with("pub fn datumesh_store::pg_unstable::with_connector<F>(")));
    assert!(!api.lines().any(|l| l == "pub use datumesh_store::postgres"));
}
