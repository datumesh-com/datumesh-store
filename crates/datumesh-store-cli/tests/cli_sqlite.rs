//! The `datumesh-store` command on SQLite: `DATABASE_URL` a file path or a `sqlite:` URL.
//! migrate (and --check), import, export, bench, each run as a process against a file of
//! its own; exports agree with PostgreSQL's for the same imports (a PostgreSQL database
//! from `DATABASE_URL`, as in cli.rs).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    format!("{prefix}_{}_{}_{nanos}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst))
}

/// A SQLite file in a directory of its own, removed afterwards.
struct Lite {
    dir: PathBuf,
    path: PathBuf,
}

impl Lite {
    fn new() -> Lite {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli-sqlite").join(unique("db"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("store.db");
        Lite { dir, path }
    }

    fn url(&self) -> String {
        self.path.to_str().unwrap().to_string()
    }

    fn run(&self, args: &[&str]) -> Output {
        run(args, Some(&self.url()), None)
    }

    fn run_stdin(&self, args: &[&str], stdin: Option<&[u8]>) -> Output {
        run(args, Some(&self.url()), stdin)
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.path).unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        self.conn().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }
}

impl Drop for Lite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A PostgreSQL database of its own on the server of `DATABASE_URL`, dropped afterwards.
struct Pg {
    name: String,
    url: String,
    admin: String,
}

impl Pg {
    fn new() -> Pg {
        use postgres::{Config, NoTls};
        let admin = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let name = unique("dm_clis");
        let mut c = admin.parse::<Config>().unwrap().connect(NoTls).unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let (base, _) = admin.rsplit_once('/').unwrap();
        Pg { url: format!("{base}/{name}"), name, admin }
    }

    fn run(&self, args: &[&str]) -> Output {
        run(args, Some(&self.url), None)
    }
}

impl Drop for Pg {
    fn drop(&mut self) {
        use postgres::{Config, NoTls};
        if let Ok(mut c) = self.admin.parse::<Config>().unwrap().connect(NoTls) {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name));
        }
    }
}

fn run(args: &[&str], url: Option<&str>, stdin: Option<&[u8]>) -> Output {
    use std::io::Write;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_datumesh-store"));
    cmd.args(args);
    cmd.env_remove("DATABASE_URL");
    cmd.env_remove("DATABASE_CA_FILE");
    if let Some(u) = url {
        cmd.env("DATABASE_URL", u);
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

/// A directory of the specification vectors `<group>/<file>` for which `keep` holds (see
/// cli.rs: sets whose key documents agree).
fn vector_set(name: &str, keep: fn(&str, &str) -> bool) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("vectors-sqlite-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for group in std::fs::read_dir(spec_vectors()).unwrap() {
        let group = group.unwrap().path();
        if !group.is_dir() {
            continue;
        }
        let g = group.file_name().unwrap().to_str().unwrap().to_string();
        for f in std::fs::read_dir(&group).unwrap() {
            let f = f.unwrap().path();
            let file = f.file_name().unwrap().to_str().unwrap().to_string();
            if keep(&g, &file) {
                std::fs::create_dir_all(dir.join(&g)).unwrap();
                std::fs::copy(&f, dir.join(&g).join(&file)).unwrap();
            }
        }
    }
    dir
}

fn vectors_without_slices() -> PathBuf {
    vector_set("no-slices", |g, f| !(g == "receipt" && f.starts_with("slice-")))
}

/// An export's lines without the namespaces' `validated_at` (the time of an import).
fn without_validation_times(export: &str) -> Vec<String> {
    export
        .lines()
        .map(|l| match l.find(r#""validated_at":""#) {
            Some(i) if l.starts_with(r#"{"namespace":"#) => format!("{}{}", &l[..i], &l[i + 43..]),
            _ => l.to_string(),
        })
        .collect()
}

#[test]
fn migrate_from_empty_rerun_and_check() {
    let db = Lite::new();
    // The file does not exist yet: migrate creates it.
    assert!(!db.path.exists());
    let out = ok(&db.run(&["migrate"]));
    assert!(out.contains("applied migration 1 (0001_initial.sql)"), "{out}");
    assert!(db.path.exists());
    let out = ok(&db.run(&["migrate"]));
    assert!(out.contains("up to date"), "{out}");
    let out = ok(&db.run(&["migrate", "--check"]));
    assert!(out.contains("no drift"), "{out}");
    // The same file as a sqlite: URL, both spellings.
    for url in [format!("sqlite:{}", db.url()), format!("sqlite://{}", db.url())] {
        let out = ok(&run(&["migrate", "--check"], Some(&url), None));
        assert!(out.contains("no drift"), "{url}: {out}");
    }

    // Drift by hand: reported, exit status 1.
    db.conn().execute_batch("DROP INDEX edges_input_hash").unwrap();
    let o = db.run(&["migrate", "--check"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    let out = text(&o.stdout);
    assert!(out.contains("- index edges_input_hash"), "{out}");
}

#[test]
fn check_of_a_missing_file_fails_and_creates_nothing() {
    let db = Lite::new();
    let o = db.run(&["migrate", "--check"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("no such database file"), "{}", text(&o.stderr));
    assert_eq!(std::fs::read_dir(&db.dir).unwrap().count(), 0, "migrate --check created a file");
}

#[test]
fn check_on_an_empty_file_reports_everything_missing() {
    let db = Lite::new();
    std::fs::write(&db.path, b"").unwrap();
    let o = db.run(&["migrate", "--check"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(text(&o.stdout).contains("- table versions"), "{}", text(&o.stdout));
}

#[test]
fn a_file_that_cannot_be_opened_is_a_connection_error() {
    let db = Lite::new();
    let missing = db.dir.join("no/such/directory/store.db");
    let o = run(&["migrate"], Some(missing.to_str().unwrap()), None);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("cannot open"), "{}", text(&o.stderr));
}

/// Import into SQLite, export, round trip into a new file; and the same import into
/// PostgreSQL exports the very same lines.
#[test]
fn import_vectors_export_round_trip_and_agree_with_postgres() {
    let db = Lite::new();
    ok(&db.run(&["migrate"]));
    let dir = vectors_without_slices();
    let out = ok(&db.run(&["import", dir.to_str().unwrap()]));
    assert!(out.contains("receipts") && out.contains("0 errors"), "{out}");
    let export1 = ok(&db.run(&["export"]));
    let lines: Vec<&str> = export1.lines().collect();
    assert!(lines.iter().filter(|l| l.starts_with(r#"{"receipt":"#)).count() >= 12);
    assert!(lines.iter().any(|l| l.starts_with(r#"{"blob":"#)));
    assert!(lines.iter().any(|l| l.starts_with(r#"{"version":"#)));
    for l in &lines {
        datumesh_core::json::parse(l.as_bytes()).unwrap();
    }
    // Re-importing is a no-op.
    ok(&db.run(&["import", dir.to_str().unwrap()]));
    assert_eq!(ok(&db.run(&["export"])), export1);

    // Into a new file, from stdin.
    let db2 = Lite::new();
    ok(&db2.run(&["migrate"]));
    ok(&db2.run_stdin(&["import", "-"], Some(export1.as_bytes())));
    assert_eq!(ok(&db2.run(&["export"])), export1);

    // PostgreSQL, the same import: the same export, but for when each import validated the
    // key documents (now, each time).
    let pg = Pg::new();
    ok(&pg.run(&["migrate"]));
    ok(&pg.run(&["import", dir.to_str().unwrap()]));
    let pg_export = ok(&pg.run(&["export"]));
    assert_eq!(without_validation_times(&pg_export), without_validation_times(&export1));
    assert_eq!(pg_export.lines().count(), export1.lines().count());
    // And PostgreSQL's export imports into SQLite as it is, and back.
    let db3 = Lite::new();
    ok(&db3.run(&["migrate"]));
    ok(&db3.run_stdin(&["import", "-"], Some(pg_export.as_bytes())));
    assert_eq!(ok(&db3.run(&["export"])), pg_export);
}

/// The 0.1.2 and 0.3.0 export fixtures import into SQLite and export the same lines.
#[test]
fn imports_of_earlier_exports() {
    for name in ["export-0.1.2.jsonl", "export-0.3.0.jsonl"] {
        let fixture = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap();
        let db = Lite::new();
        ok(&db.run(&["migrate"]));
        let out = ok(&db.run_stdin(&["import", "-"], Some(&fixture)));
        assert!(out.contains("0 errors"), "{name}: {out}");
        assert_eq!(ok(&db.run(&["export"])), text(&fixture), "{name}");
        let unindexed: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM receipts r WHERE NOT EXISTS (SELECT 1 FROM cells c WHERE c.cell_id = r.cell_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unindexed, 0, "{name}");
    }
}

#[test]
fn export_and_import_keep_manifests_computed_versions_and_slice_lineage() {
    let db = Lite::new();
    ok(&db.run(&["migrate"]));
    let dir = vector_set("slices", |g, f| (g == "receipt" && f.starts_with("slice-")) || g == "slice");
    let out = ok(&db.run(&["import", dir.to_str().unwrap()]));
    assert!(out.contains("manifests"), "{out}");
    // Every stored receipt as a computed version of its cell (one per key), in raw SQL.
    let n = db
        .conn()
        .execute(
            "INSERT INTO computed_versions (cell_id, tx, asof, receipt_hash)
             SELECT cell_id, tx, asof, min(hash) FROM receipts GROUP BY cell_id, tx, asof",
            [],
        )
        .unwrap();
    assert!(n >= 5, "{n}");
    let export1 = ok(&db.run(&["export"]));
    let lines: Vec<&str> = export1.lines().collect();
    assert!(lines.iter().filter(|l| l.starts_with(r#"{"manifest":""#)).count() >= 10);
    assert_eq!(lines.iter().filter(|l| l.starts_with(r#"{"computed":{"#)).count(), n);
    let tables = ["manifests", "manifest_members", "slice_edges", "cells", "cell_args", "computed_versions"];
    let want: Vec<i64> = tables.iter().map(|t| db.count(t)).collect();
    assert!(want.iter().all(|n| *n > 0), "{want:?}");

    let db2 = Lite::new();
    ok(&db2.run(&["migrate"]));
    ok(&db2.run_stdin(&["import", "-"], Some(export1.as_bytes())));
    assert_eq!(ok(&db2.run(&["export"])), export1);
    assert_eq!(tables.iter().map(|t| db2.count(t)).collect::<Vec<_>>(), want);
    ok(&db2.run_stdin(&["import", "-"], Some(export1.as_bytes())));
    assert_eq!(ok(&db2.run(&["export"])), export1);
    assert_eq!(tables.iter().map(|t| db2.count(t)).collect::<Vec<_>>(), want);
}

#[test]
fn bench_runs_small() {
    let db = Lite::new();
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
        "--slice-cells",
        "200",
    ]));
    for op in [
        "version_asof",
        "receipt",
        "put_version",
        "dependents",
        "computed_asof",
        "computed_asof_with_receipt",
        "stored_asof_with_receipt",
        "put_computed",
        "cell_dependents",
        "resolve_binding",
        "slice_members",
        "slice_values",
        "slice_changed_after",
        "versions_between",
        "put_manifest",
        "manifest",
        "disk",
        "blob",
    ] {
        assert!(out.contains(op), "{op}: {out}");
    }
    assert!(out.contains("p50") && out.contains("p99"), "{out}");
    assert_eq!(db.count("versions"), 200 + 50 + 200 + 200);
    assert!(out.contains("200 members"), "{out}");
    assert_eq!(db.count("computed_versions"), 200 + 10 + 50);
    assert!(out.contains("10 dependent cells"), "{out}");
    let out = ok(&db.run(&["bench", "--skip-load", "--samples", "10", "--blob-max", "0"]));
    assert!(out.contains("version_asof") && out.contains("10 dependent cells"), "{out}");
    let out = ok(&db.run(&["bench", "--wait"]));
    assert!(out.contains("answered"), "{out}");
}
