//! The `datumesh-store` command: migrate (and --check), import, export, bench, each run as
//! a process against a database of its own (created from `DATABASE_URL`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use postgres::{Client, Config, NoTls};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Db {
    name: String,
    url: String,
    admin: String,
}

impl Db {
    fn new() -> Db {
        Db::new_on(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
    }

    /// A database on the server of `admin` (plain-text clients: `NoTls`).
    fn new_on(admin: &str) -> Db {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let name = format!("dm_cli_{}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst), nanos);
        let mut c = admin.parse::<Config>().unwrap().connect(NoTls).unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let (base, _) = admin.rsplit_once('/').unwrap();
        Db { url: format!("{base}/{name}"), name, admin: admin.to_string() }
    }

    fn client(&self) -> Client {
        self.url.parse::<Config>().unwrap().connect(NoTls).unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_stdin(args, None)
    }

    fn run_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        run(args, Some(&self.url), stdin)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(mut c) = self.admin.parse::<Config>().unwrap().connect(NoTls) {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name));
        }
    }
}

fn run(args: &[&str], url: Option<&str>, stdin: Option<&[u8]>) -> Output {
    run_env(args, url, stdin, &[])
}

fn run_env(args: &[&str], url: Option<&str>, stdin: Option<&[u8]>, env: &[(&str, &str)]) -> Output {
    use std::io::Write;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_datumesh-store"));
    cmd.args(args);
    cmd.env_remove("DATABASE_URL");
    cmd.env_remove("DATABASE_CA_FILE");
    if let Some(u) = url {
        cmd.env("DATABASE_URL", u);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        let mut si = child.stdin.take().unwrap();
        if let Some(b) = stdin {
            si.write_all(b).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn ok(o: &Output) -> String {
    assert!(o.status.success(), "status {:?}\nstdout:\n{}\nstderr:\n{}", o.status, text(&o.stdout), text(&o.stderr));
    text(&o.stdout)
}

fn spec_vectors() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

#[test]
fn migrate_from_empty_rerun_and_check() {
    let db = Db::new();
    let out = ok(&db.run(&["migrate"]));
    assert!(out.contains("applied migration 1"), "{out}");
    let out = ok(&db.run(&["migrate"]));
    assert!(out.contains("up to date"), "{out}");
    let out = ok(&db.run(&["migrate", "--check"]));
    assert!(out.contains("no drift"), "{out}");

    // Drift by hand: reported, exit status 1.
    db.client().batch_execute("ALTER TABLE receipts ADD COLUMN note text").unwrap();
    let o = db.run(&["migrate", "--check"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("+ column receipts.note"), "{out}");
}

#[test]
fn check_on_an_empty_database_reports_everything_missing() {
    let db = Db::new();
    let o = db.run(&["migrate", "--check"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stdout).contains("- relation r versions"), "{}", text(&o.stdout));
}

#[test]
fn import_vectors_export_and_round_trip() {
    let db = Db::new();
    ok(&db.run(&["migrate"]));
    let dir = spec_vectors();
    let out = ok(&db.run(&["import", dir.to_str().unwrap()]));
    assert!(out.contains("receipts"), "{out}");
    let export1 = ok(&db.run(&["export"]));
    let lines: Vec<&str> = export1.lines().collect();
    assert!(lines.iter().any(|l| l.starts_with(r#"{"descriptor":"#)), "{export1}");
    assert!(lines.iter().filter(|l| l.starts_with(r#"{"receipt":"#)).count() >= 12);
    assert!(lines.iter().any(|l| l.starts_with(r#"{"blob":"#)));
    assert!(lines.iter().any(|l| l.starts_with(r#"{"version":"#)));
    // The vectors' key documents are imported as namespaces, and exported before the
    // receipts they verify.
    let first_ns = lines.iter().position(|l| l.starts_with(r#"{"namespace":"#)).expect("namespaces exported");
    let first_receipt = lines.iter().position(|l| l.starts_with(r#"{"receipt":"#)).unwrap();
    assert!(first_ns < first_receipt);
    // Every line is JSON.
    for l in &lines {
        datumesh_core::json::parse(l.as_bytes()).unwrap();
    }

    // Re-importing is a no-op.
    ok(&db.run(&["import", dir.to_str().unwrap()]));
    assert_eq!(ok(&db.run(&["export"])), export1);

    // The export imports into an empty database and exports the same (from stdin and from
    // a file).
    let db2 = Db::new();
    ok(&db2.run(&["migrate"]));
    ok(&db2.run_stdin(&["import", "-"], Some(export1.as_bytes())));
    assert_eq!(ok(&db2.run(&["export"])), export1);
    ok(&db2.run_stdin(&["import", "-"], Some(export1.as_bytes())));
    assert_eq!(ok(&db2.run(&["export"])), export1);
}

#[test]
fn import_reports_bad_lines_and_fails() {
    let db = Db::new();
    ok(&db.run(&["migrate"]));
    let input = b"{\"receipt\":\"{}\"}\nnot json\n{\"unknown\":1}\n";
    let o = db.run_stdin(&["import", "-"], Some(input));
    assert_eq!(o.status.code(), Some(1));
    let err = text(&o.stderr);
    assert!(err.contains("line 1") && err.contains("E_SCHEMA"), "{err}");
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("line 3"), "{err}");
}

#[test]
fn bench_runs_small() {
    let db = Db::new();
    ok(&db.run(&["migrate"]));
    let out = ok(&db.run(&[
        "bench",
        "--cells",
        "20",
        "--versions",
        "200",
        "--dependents",
        "10",
        "--samples",
        "50",
        "--blob-max",
        "65536",
    ]));
    for op in ["version_asof", "receipt", "put_version", "dependents", "disk", "blob"] {
        assert!(out.contains(op), "{op}: {out}");
    }
    assert!(out.contains("p50") && out.contains("p99"), "{out}");
    let n: i64 = db.client().query_one("SELECT count(*) FROM versions", &[]).unwrap().get(0);
    assert_eq!(n, 200 + 50);

    // A second run on the loaded database, without loading.
    let out = ok(&db.run(&["bench", "--skip-load", "--samples", "10", "--blob-max", "0"]));
    assert!(out.contains("version_asof") && out.contains("dependents"), "{out}");
    // The probe used for cold-restart timing answers.
    let out = ok(&db.run(&["bench", "--wait"]));
    assert!(out.contains("answered"), "{out}");
}

#[test]
fn usage_errors_exit_2() {
    for args in [&["frobnicate"][..], &[][..], &["migrate", "--nope"][..]] {
        let o = run(args, Some("postgres://x@127.0.0.1:1/x"), None);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(text(&o.stderr).contains("usage"), "{args:?}: {}", text(&o.stderr));
    }
    let o = run(&["migrate"], None, None);
    assert_eq!(o.status.code(), Some(2));
    assert!(text(&o.stderr).contains("DATABASE_URL"));
}

#[test]
fn import_refuses_receipts_without_their_namespace() {
    let db = Db::new();
    ok(&db.run(&["migrate"]));
    let v =
        datumesh_core::json::parse(&std::fs::read(spec_vectors().join("receipt/stored-number.json")).unwrap()).unwrap();
    let receipt = v.get("input").unwrap().clone();
    let line = datumesh_core::json::Value::Object(vec![("receipt".into(), receipt)]).to_jcs();
    let o = db.run_stdin(&["import", "-"], Some(&line));
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("E_KEY_UNAVAILABLE"), "{}", text(&o.stderr));
}

/// Every command over TLS: `DATABASE_URL` with `sslmode=require` (TLS or nothing) and the
/// server's CA in `DATABASE_CA_FILE`; without the CA the server's certificate is refused.
/// Needs the TLS server of tools/tls_pg.sh (`DATABASE_TLS_URL`, `DATABASE_TLS_CA_FILE`);
/// skipped without it unless `DATUMESH_REQUIRE_TLS_TESTS` is set.
#[test]
fn every_command_works_over_tls_with_the_ca() {
    let (admin, ca) = match (std::env::var("DATABASE_TLS_URL"), std::env::var("DATABASE_TLS_CA_FILE")) {
        (Ok(u), Ok(c)) => (u, c),
        _ => {
            assert!(std::env::var_os("DATUMESH_REQUIRE_TLS_TESTS").is_none(), "TLS server not configured");
            eprintln!("skipped: DATABASE_TLS_URL / DATABASE_TLS_CA_FILE not set (tools/tls_pg.sh)");
            return;
        }
    };
    let db = Db::new_on(&admin);
    let url = format!("{}?sslmode=require", db.url);
    let with_ca = [("DATABASE_CA_FILE", ca.as_str())];
    let tls = |args: &[&str]| run_env(args, Some(&url), None, &with_ca);

    // Without the CA: the certificate is refused, a connection error (exit 2).
    let o = run(&["migrate"], Some(&url), None);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("certificate"), "{}", text(&o.stderr));
    // A CA file that does not exist, or holds no certificate: exit 2.
    let o = run_env(&["migrate"], Some(&url), None, &[("DATABASE_CA_FILE", "/nonexistent/ca.pem")]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("DATABASE_CA_FILE"), "{}", text(&o.stderr));

    let out = ok(&tls(&["migrate"]));
    assert!(out.contains("applied migration 1"), "{out}");
    let out = ok(&tls(&["migrate", "--check"]));
    assert!(out.contains("no drift"), "{out}");
    let vectors = spec_vectors();
    ok(&tls(&["import", vectors.to_str().unwrap()]));
    let export = ok(&tls(&["export"]));
    assert!(export.lines().any(|l| l.starts_with("{\"receipt\"")), "{export}");
    let out = ok(&tls(&[
        "bench",
        "--cells",
        "2",
        "--versions",
        "4",
        "--dependents",
        "2",
        "--samples",
        "3",
        "--threads",
        "2",
        "--blob-max",
        "1024",
    ]));
    assert!(out.contains("version_asof"), "{out}");
    let out = ok(&tls(&["bench", "--wait"]));
    assert!(out.contains("answered"), "{out}");
}
