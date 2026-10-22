//! TLS: `connect` / `connect_with` always use the rustls connector; the URL's `sslmode`
//! decides whether TLS is used, and when it is, the server's certificate and host name are
//! verified against the Mozilla roots plus the roots added through `PoolOptions`.
//!
//! The server tests need `DATABASE_TLS_URL` (a PostgreSQL server with `ssl=on`, whose
//! certificate is valid for the URL's host name and issued by the CA in
//! `DATABASE_TLS_CA_FILE`, and which also accepts plain-text connections) and
//! `DATABASE_TLS_CA_FILE`; `tools/tls_pg.sh` starts one. Without them they are skipped,
//! unless `DATUMESH_REQUIRE_TLS_TESTS` is set (CI sets all three), which makes them fail.

mod common;

use std::net::ToSocketAddrs;

use common::*;
use datumesh_store::pg_unstable::postgres::{Config, NoTls};
use datumesh_store::{migrate, pg_unstable, Error, PgStore, PoolOptions, Resolver, Store};

/// The TLS server's admin URL (plain text: `NoTls` clients never use TLS under the default
/// `sslmode=prefer`) and the CA's PEM, or None (skipped).
fn tls_server() -> Option<(String, Vec<u8>)> {
    match (std::env::var("DATABASE_TLS_URL"), std::env::var("DATABASE_TLS_CA_FILE")) {
        (Ok(url), Ok(ca)) => {
            let pem = std::fs::read(&ca).unwrap_or_else(|e| panic!("DATABASE_TLS_CA_FILE {ca}: {e}"));
            Some((url, pem))
        }
        _ => {
            assert!(
                std::env::var_os("DATUMESH_REQUIRE_TLS_TESTS").is_none(),
                "DATUMESH_REQUIRE_TLS_TESTS is set, but DATABASE_TLS_URL or DATABASE_TLS_CA_FILE is not"
            );
            eprintln!("skipped: DATABASE_TLS_URL / DATABASE_TLS_CA_FILE not set (tools/tls_pg.sh)");
            None
        }
    }
}

fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{prefix}_{}_{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

/// Whether the server's sessions with this application name use TLS, as the server sees
/// them (`pg_stat_ssl`); None when there is no such session.
fn sessions_use_tls(db: &TestDb, app: &str) -> Option<bool> {
    db.client()
        .query_one(
            "SELECT bool_and(s.ssl) FROM pg_stat_ssl s JOIN pg_stat_activity a USING (pid) WHERE a.application_name = $1",
            &[&app],
        )
        .unwrap()
        .get(0)
}

/// Stores the vector's key documents and receipt through `s` and reads the receipt back.
fn store_works(s: &PgStore) {
    migrate::apply(s).unwrap();
    put_vector_keys(s, &vector("receipt/stored-number"));
    let bytes = input(&vector("receipt/stored-number"));
    let h = s.put_receipt(&bytes).unwrap();
    assert_eq!(s.receipt(&h).map(|b| b.into_owned()), Some(jcs(&bytes)));
}

fn database_message(e: &Error) -> String {
    match e {
        Error::Database(d) => {
            assert_eq!(d.code(), None, "{e}");
            assert!(std::error::Error::source(d).is_some());
            d.message().to_string()
        }
        other => panic!("not a database error: {other:?}"),
    }
}

#[test]
fn require_with_the_ca_connects_verifies_and_stores() {
    let Some((admin, ca)) = tls_server() else { return };
    let db = TestDb::empty_on(&admin);
    let app = unique("dm_tls_require");
    let url = with_param(&db.url(), &format!("sslmode=require&application_name={app}"));
    let options = PoolOptions::new().tls_root_certificates_pem(&ca).unwrap();
    let s = PgStore::connect_with(&url, options.clone()).unwrap();
    store_works(&s);
    // The server sees the store's connection as TLS.
    assert_eq!(sessions_use_tls(&db, &app), Some(true));

    // The same connector, as a client of its own: the session itself says it is TLS.
    let config: Config = url.parse().unwrap();
    let mut c = pg_unstable::Connector::new(config, &options).unwrap().connect().unwrap();
    let ssl: bool = c.query_one("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", &[]).unwrap().get(0);
    assert!(ssl);

    // The default sslmode (prefer) uses TLS too when the server offers it.
    let app = unique("dm_tls_prefer");
    let s = PgStore::connect_with(&with_param(&db.url(), &format!("application_name={app}")), options).unwrap();
    assert!(s.check_statements().unwrap() >= 20);
    assert_eq!(sessions_use_tls(&db, &app), Some(true));

    // Only the given roots (no Mozilla roots): a private CA still verifies.
    let only_ca = PoolOptions::new().tls_webpki_roots(false).tls_root_certificates_pem(&ca).unwrap();
    let s = PgStore::connect_with(&with_param(&db.url(), "sslmode=require"), only_ca).unwrap();
    assert!(s.check_statements().unwrap() >= 20);
}

#[test]
fn require_without_the_ca_fails_on_the_certificate() {
    let Some((admin, _)) = tls_server() else { return };
    let db = TestDb::empty_on(&admin);
    let url = with_param(&db.url(), "sslmode=require");
    for e in [
        PgStore::connect(&url).unwrap_err(),
        PgStore::connect_with(&url, PoolOptions::new()).unwrap_err(),
        // Without any root at all.
        PgStore::connect_with(&url, PoolOptions::new().tls_webpki_roots(false)).unwrap_err(),
        // The default sslmode (prefer) does not fall back to plain text when the
        // certificate does not verify: the documented consequence for self-signed servers.
        PgStore::connect(&db.url()).unwrap_err(),
    ] {
        let m = database_message(&e);
        assert!(m.to_lowercase().contains("certificate"), "{m}");
        assert!(e.to_string().to_lowercase().contains("certificate"), "{e}");
    }
}

#[test]
fn a_wrong_host_name_fails() {
    let Some((admin, ca)) = tls_server() else { return };
    let db = TestDb::empty_on(&admin);
    // The same server by IP address: the certificate names hosts only.
    let config: Config = db.url().parse().unwrap();
    let host = match &config.get_hosts()[0] {
        pg_unstable::postgres::config::Host::Tcp(h) => h.clone(),
        other => panic!("DATABASE_TLS_URL must name a TCP host: {other:?}"),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    let ip = (host.as_str(), port)
        .to_socket_addrs()
        .unwrap()
        .find(|a| a.is_ipv4())
        .expect("an IPv4 address for the TLS server")
        .ip()
        .to_string();
    let url = db.url().replacen(&format!("@{host}:"), &format!("@{ip}:"), 1);
    assert_ne!(url, db.url(), "DATABASE_TLS_URL must have the form postgres://user:password@host:port/db");
    let options = PoolOptions::new().tls_root_certificates_pem(&ca).unwrap();
    // Plain text by IP works: the address is right, only the name is wrong.
    PgStore::connect_with(&with_param(&url, "sslmode=disable"), options.clone()).unwrap();
    let e = PgStore::connect_with(&with_param(&url, "sslmode=require"), options).unwrap_err();
    let m = database_message(&e);
    assert!(m.to_lowercase().contains("certificate") && m.contains("name"), "{m}");
}

#[test]
fn disable_connects_in_plain_text() {
    let Some((admin, _)) = tls_server() else { return };
    let db = TestDb::empty_on(&admin);
    let app = unique("dm_tls_disable");
    // No CA given: with sslmode=disable no certificate is looked at.
    let s = PgStore::connect(&with_param(&db.url(), &format!("sslmode=disable&application_name={app}"))).unwrap();
    store_works(&s);
    assert_eq!(sessions_use_tls(&db, &app), Some(false));
    // And a plain client agrees (NoTls never asks for TLS).
    let mut c = db.config.connect(NoTls).unwrap();
    let ssl: bool = c.query_one("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()", &[]).unwrap().get(0);
    assert!(!ssl);
}

/// The development database (`DATABASE_URL`) has no TLS: `connect` with the default
/// `sslmode=prefer` connects in plain text there.
#[test]
fn prefer_connects_in_plain_text_to_a_server_without_tls() {
    let db = TestDb::empty();
    let ssl: String = db.client().query_one("SHOW ssl", &[]).unwrap().get(0);
    if ssl != "off" {
        eprintln!("skipped: DATABASE_URL's server has ssl = {ssl}");
        return;
    }
    let app = unique("dm_plain_prefer");
    let s = PgStore::connect(&with_param(&db.url(), &format!("application_name={app}"))).unwrap();
    assert!(migrate::catalog(&s).unwrap().is_empty());
    assert_eq!(sessions_use_tls(&db, &app), Some(false));
}

/// Root certificates come as a PEM bundle; one with no certificate in it, or one that does
/// not parse, is refused when given (`Error::Config`), not at connect time.
#[test]
fn root_certificate_bundles_are_checked() {
    let refused = |pem: &[u8]| match PoolOptions::new().tls_root_certificates_pem(pem) {
        Err(Error::Config(m)) => m,
        other => panic!("{:?}", other.map(|_| ())),
    };
    refused(b"");
    refused(b"not PEM at all");
    // A PEM section of another kind only.
    refused(b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----\n");
    // A certificate section that is not base64, or not a certificate.
    refused(b"-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n");
    refused(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n");
    let e = Error::Config("x".into());
    assert!(e.to_string().contains('x'), "{e}");
    // Builder style, comparable, cloneable, as before.
    let a = PoolOptions::new().max_size(2).tls_webpki_roots(false);
    assert_eq!(a.clone(), a);
    assert_ne!(a, PoolOptions::new());
    if let Some((_, ca)) = tls_server() {
        let one = PoolOptions::new().tls_root_certificates_pem(&ca).unwrap();
        // Bundles append: the CA twice, or with another bundle, is accepted.
        let mut two = ca.clone();
        two.extend_from_slice(&ca);
        one.clone().tls_root_certificates_pem(&two).unwrap();
        assert_ne!(one, PoolOptions::new());
    }
}
