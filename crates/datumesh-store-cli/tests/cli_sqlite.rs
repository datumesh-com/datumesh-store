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
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    // One directory per call: tests of one process run in parallel and must not remove each other's.
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "vectors-sqlite-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    ));
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

///
/// And without receipt/relayed-full (spec-0.7.0): the vectors are independent, and it shares a
/// cell and tx with stored-number (both stored versions of the EUR/USD cell at 2024-03-15T16:00,
/// different receipts), so a single store cannot hold both (NotLater, correctly). The import
/// puts only valued receipts as versions, so the withdrawn-* vectors are imported as receipts
/// and conflict with nothing.
fn vectors_without_slices() -> PathBuf {
    vector_set("no-slices", |g, f| !(g == "receipt" && (f.starts_with("slice-") || f == "relayed-full.json")))
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

// ---- Classes (0.4.1) ---------------------------------------------------------------------
//
// The command has no put/bind subcommands for functions, so it has none for classes (the
// contract's item 5 asks for them only "if the CLI has the matching function commands"). What
// it does have, export and import, must not lose classes: a class descriptor and a class
// binding (with the method bindings it made) go through an export and an import unchanged.
// The line formats are the implementation's to choose (see IMPL-NOTES-0.4.1-classes.md).

mod classes {
    use super::*;
    use datumesh_store::{PgStore, SqliteStore, Store};

    const CLASS: &str = "rates.example.org/fx_pair";
    const T1: &str = "2024-03-15T16:00:00.000000Z";
    const T2: &str = "2024-03-18T10:00:00.000000Z";
    const END: &str = "9999-12-31T23:59:59.999999Z";

    fn method_fn(tag: &str) -> Vec<u8> {
        let key = r#"{"key_normalisation":"upper_ascii","type":"string","valid":false}"#;
        format!(
            r#"{{"code":{{"body":"{tag}","lang":"stored"}},"inputs":[],"signature":{{"args":{{"base":{key},"date":{{"type":"date","valid":true}},"quote":{key}}},"order":["base","quote","date"],"returns":{{"type":"number","unit":"1"}}}}}}"#
        )
        .into_bytes()
    }

    /// (flattened canonical bytes, class hash, function descriptors, method name -> function hash)
    type Fx = (Vec<u8>, String, Vec<Vec<u8>>, Vec<(String, String)>);

    fn fx(version: &str) -> Fx {
        let fns = [("rate", method_fn(&format!("rate {version}"))), ("spot", method_fn("spot"))];
        let mut env = datumesh_core::ClassEnv::new("rates.example.org");
        let mut methods = Vec::new();
        for (m, f) in &fns {
            let d = datumesh_core::FunctionDescriptor::from_json(f).unwrap();
            methods.push((m.to_string(), d.hash()));
            env.insert_function(d);
        }
        let text = format!(
            r#"{{"keys":{{"base":{{"key_normalisation":"upper_ascii","type":"string"}},"quote":{{"key_normalisation":"upper_ascii","type":"string"}}}},"methods":{{"rate":"{}","spot":"{}"}},"order":["base","quote"],"steward":{{"namespace":"rates.example.org","process":"https://rates.example.org/classes/change"}}}}"#,
            methods[0].1, methods[1].1
        );
        let flat = datumesh_core::define_class(CLASS, text.as_bytes(), &env).unwrap();
        (flat.canonical(), flat.hash(), fns.into_iter().map(|(_, f)| f).collect(), methods)
    }

    /// Fills a store: a function name, then the class bound at T1 and rebound (a new `rate`) at T2.
    fn fill(s: &dyn Store) {
        let (_, h1, fns1, _) = fx("v1");
        let (_, h2, fns2, _) = fx("v2");
        for f in fns1.iter().chain(&fns2) {
            s.put_descriptor(f).unwrap();
        }
        s.bind_name(
            "rates.example.org/fx_rate",
            &datumesh_core::FunctionDescriptor::from_json(&fns1[0]).unwrap().hash(),
            T1,
        )
        .unwrap();
        for (bytes, hash) in [(fx("v1").0, &h1), (fx("v2").0, &h2)] {
            assert_eq!(&s.put_class(&bytes).unwrap(), hash);
        }
        s.bind_class(CLASS, &h1, T1).unwrap();
        s.bind_class(CLASS, &h2, T2).unwrap();
    }

    /// What `fill` made is in `s`.
    fn check(s: &dyn Store) {
        let (b1, h1, _, m1) = fx("v1");
        let (b2, h2, _, m2) = fx("v2");
        assert_eq!(s.class(&h1).unwrap(), Some(b1));
        assert_eq!(s.class(&h2).unwrap(), Some(b2));
        assert_eq!(s.resolve_class(CLASS, T1).unwrap(), Some(h1));
        assert_eq!(s.resolve_class(CLASS, END).unwrap(), Some(h2));
        let tx = |asof: &str| s.resolve_class_binding(CLASS, asof).unwrap().unwrap().tx;
        assert_eq!((tx(T1).as_str(), tx(END).as_str()), (T1, T2));
        let rate = |asof: &str| s.resolve_name(&format!("{CLASS}/rate"), asof).unwrap();
        assert_eq!(rate(T1).as_deref(), Some(m1[0].1.as_str()));
        assert_eq!(rate(END).as_deref(), Some(m2[0].1.as_str()));
        assert_ne!(m1[0].1, m2[0].1);
        assert_eq!(s.resolve_name(&format!("{CLASS}/spot"), END).unwrap().as_deref(), Some(m2[1].1.as_str()));
        // The function name next to them is still a function.
        assert!(s.resolve_name("rates.example.org/fx_rate", END).unwrap().is_some());
        assert_eq!(s.resolve_class("rates.example.org/fx_rate", END).unwrap(), None);
    }

    #[test]
    fn export_and_import_keep_classes_and_class_bindings() {
        let a = Lite::new();
        ok(&a.run(&["migrate"]));
        let store = SqliteStore::open(&a.path).unwrap();
        fill(&store);
        check(&store);
        let export1 = ok(&a.run(&["export"]));
        for l in export1.lines() {
            datumesh_core::json::parse(l.as_bytes()).unwrap();
        }
        // The export says something about classes: their descriptors are in it.
        assert!(export1.contains(&fx("v1").1) && export1.contains(&fx("v2").1), "{export1}");

        // Into a new SQLite file: the same store, the same export; again: a no-op.
        let b = Lite::new();
        ok(&b.run(&["migrate"]));
        let out = ok(&b.run_stdin(&["import", "-"], Some(export1.as_bytes())));
        assert!(out.contains("0 errors"), "{out}");
        check(&SqliteStore::open(&b.path).unwrap());
        assert_eq!(ok(&b.run(&["export"])), export1);
        let out = ok(&b.run_stdin(&["import", "-"], Some(export1.as_bytes())));
        assert!(out.contains("0 errors"), "{out}");
        assert_eq!(ok(&b.run(&["export"])), export1);

        // Into PostgreSQL, and back: the same lines from either backend.
        let pg = Pg::new();
        ok(&pg.run(&["migrate"]));
        let out = ok(&pg.run_stdin_pg(&["import", "-"], export1.as_bytes()));
        assert!(out.contains("0 errors"), "{out}");
        check(&PgStore::connect(&pg.url).unwrap());
        let pg_export = ok(&pg.run(&["export"]));
        assert_eq!(pg_export, export1);
        let c = Lite::new();
        ok(&c.run(&["migrate"]));
        ok(&c.run_stdin(&["import", "-"], Some(pg_export.as_bytes())));
        assert_eq!(ok(&c.run(&["export"])), export1);
    }
}

impl Pg {
    fn run_stdin_pg(&self, args: &[&str], stdin: &[u8]) -> Output {
        run(args, Some(&self.url), Some(stdin))
    }
}
