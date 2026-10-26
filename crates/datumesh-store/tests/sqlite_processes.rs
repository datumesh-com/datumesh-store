//! SQLite: the append-only guarantees hold under concurrent writers from the same machine,
//! several processes with several threads each, on one database file (the brief's stop
//! condition). Each process is this test binary run again (`child`, selected by an
//! environment variable), started together; the parent then checks what they did.
//!
//! * Versions of one cell, appended through `put_version` with interleaved tx values from
//!   every thread of every process: every success is later than everything committed
//!   before it (the insertion order, `rowid`, is tx order), every failure is `NotLater`,
//!   and nothing else fails (no `SQLITE_BUSY` reaches a caller: the busy timeout waits).
//! * The same for name bindings (`bind_name`), and for raw SQL inserts into `versions` of
//!   another cell from a thread of each process (the triggers alone decide).
//! * Computed versions: per key, exactly one of the rival receipts wins, everywhere; the
//!   others are `Conflict`.
//! * One receipt and one manifest put by everyone: stored once, every put `Ok`.
//! * Migrations applied by several processes at once on an empty file: applied once.

#[macro_use]
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;
use datumesh_core::{Manifest, Receipt, Slice};
use datumesh_store::{migrate, Error, SqliteOptions, SqliteStore, Store};

const PROCESSES: usize = 4;
const THREADS: usize = 4;
const ATTEMPTS: usize = 25;
const ROUNDS: usize = 5;
const NAME: &str = "rates.example.org/fx_rate";
const RAW_CELL: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// The tx of the `j`th append: minute `j` of 2024-03-15.
fn tx(j: usize) -> String {
    format!("2024-03-15T{:02}:{:02}:00.000000Z", j / 60, j % 60)
}

/// The global ordinal of attempt `k` of thread `t` of process `p`: each writer's sequence
/// increases, interleaved with every other writer's.
fn ordinal(p: usize, t: usize, k: usize) -> usize {
    k * PROCESSES * THREADS + p * THREADS + t
}

/// The receipt of the version appended at ordinal `j`.
fn version_receipt(j: usize) -> Vec<u8> {
    stored_number_at(&format!("{}", 1000 + j), &tx(j))
}

/// The computed-version key of round `r` (a tx on 2024-04-01), and the rival receipt of
/// writer (p, t) for it.
fn rival(r: usize, p: usize, t: usize) -> Vec<u8> {
    let at = format!("2024-04-01T{r:02}:00:00.000000Z");
    stored_number_at(&format!("7.{r}{p}{t}1"), &at)
}

fn shared_manifest() -> Manifest {
    let sl = Slice::from_json(
        br#"{"args":{"k":"*"},"fn":"f","fn_hash":"0000000000000000000000000000000000000000000000000000000000000000","ns":"example.org"}"#,
    )
    .unwrap();
    Manifest::new(sl, (0..1000u64).map(|i| format!("{:064x}", i * 3 + 1)).collect()).unwrap()
}

/// What one writer did, one line each: `<what> <outcome> <detail>`.
fn record(lines: &mut Vec<String>, what: &str, r: Result<(), Error>, detail: &str) {
    let outcome = match r {
        Ok(()) => "ok".to_string(),
        Err(Error::NotLater { .. }) => "notlater".to_string(),
        Err(Error::Conflict { .. }) => "conflict".to_string(),
        Err(e) => format!("error:{e}"),
    };
    lines.push(format!("{what} {outcome} {detail}"));
}

/// The child process: waits for the go file, then runs its writer threads; writes what
/// they did to its result file. A no-op in an ordinary test run.
#[test]
fn child() {
    let (Ok(path), Ok(p)) = (std::env::var("DM_SQLITE_CHILD_DB"), std::env::var("DM_SQLITE_CHILD_INDEX")) else {
        return;
    };
    let p: usize = p.parse().unwrap();
    let path = PathBuf::from(path);
    let dir = path.parent().unwrap().to_path_buf();
    if std::env::var("DM_SQLITE_CHILD_MIGRATE").is_ok() {
        wait_for(&dir.join("go"));
        let applied = migrate::sqlite::apply(&SqliteStore::open(&path).unwrap()).unwrap();
        std::fs::write(dir.join(format!("child-{p}.txt")), format!("migrate {}\n", applied.len())).unwrap();
        return;
    }
    let store = SqliteStore::open_with(&path, SqliteOptions::new().max_size(THREADS)).unwrap();
    let cell = Receipt::from_json(&version_receipt(0)).unwrap().cell().id();
    let fn_hash = datumesh_core::function_hash(&fx_rate_descriptor()).unwrap();
    // Signed before the start, so that the writers race on the store alone.
    let rivals: Vec<Vec<Vec<u8>>> = (0..THREADS).map(|t| (0..ROUNDS).map(|r| rival(r, p, t)).collect()).collect();
    let same_receipt = stored_number_at("9.99", "2024-05-01T00:00:00.000000Z");
    let manifest = shared_manifest().canonical();
    wait_for(&dir.join("go"));
    let lines: Vec<String> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let (store, cell, fn_hash, rivals, same, manifest) =
                (&store, &cell, &fn_hash, &rivals[t], &same_receipt, &manifest);
            handles.push(scope.spawn(move || {
                let mut lines = Vec::new();
                for k in 0..ATTEMPTS {
                    let j = ordinal(p, t, k);
                    let h = Receipt::from_json(&version_receipt(j)).unwrap().hash().to_string();
                    record(&mut lines, "version", store.put_version(cell, &tx(j), &h), &tx(j));
                    record(&mut lines, "name", store.bind_name(NAME, fn_hash, &tx(j)), &tx(j));
                    if k < ROUNDS {
                        let r = store.put_receipt(&rivals[k]).map(|_| ());
                        record(&mut lines, "rival_receipt", r, "");
                        let rr = Receipt::from_json(&rivals[k]).unwrap();
                        record(&mut lines, &format!("computed{k}"), store.put_computed(cell, rr.tx(), rr.hash()), rr.hash());
                    }
                    if k % 5 == 0 {
                        record(&mut lines, "same_receipt", store.put_receipt(same).map(|_| ()), "");
                        record(&mut lines, "manifest", store.put_manifest(manifest).map(|_| ()), "");
                    }
                }
                lines
            }));
        }
        // One more thread: raw inserts into versions of another cell, through a connection
        // of its own, with no store in between (the triggers decide).
        handles.push(scope.spawn(move || {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.busy_timeout(Duration::from_secs(60)).unwrap();
            let h = Receipt::from_json(&version_receipt(0)).unwrap().hash().to_string();
            let mut lines = Vec::new();
            for k in 0..ATTEMPTS * 2 {
                let j = k * PROCESSES + p;
                let r = c.execute(
                    "INSERT INTO versions (cell_id, tx, receipt_hash) VALUES (?1, ?2, ?3)",
                    rusqlite::params![RAW_CELL, tx(j), h],
                );
                let outcome = match r {
                    Ok(_) => "ok".to_string(),
                    Err(e) if e.to_string().contains("not later") => "notlater".to_string(),
                    Err(e) => format!("error:{e}"),
                };
                lines.push(format!("raw {outcome} {}", tx(j)));
            }
            lines
        }));
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    std::fs::write(dir.join(format!("child-{p}.txt")), lines.join("\n") + "\n").unwrap();
}

fn wait_for(go: &Path) {
    let start = Instant::now();
    while !go.exists() {
        assert!(start.elapsed() < Duration::from_secs(120), "no go file");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Starts `PROCESSES` children on `db` (with `extra` environment), creates the go file once
/// they are all running, and returns each child's result lines.
fn run_children(db: &SqliteDb, extra: &[(&str, &str)]) -> Vec<Vec<String>> {
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..PROCESSES)
        .map(|p| {
            let mut cmd = Command::new(&exe);
            cmd.args(["--exact", "child", "--test-threads=1", "--quiet"])
                .env("DM_SQLITE_CHILD_DB", &db.path)
                .env("DM_SQLITE_CHILD_INDEX", p.to_string());
            for (k, v) in extra {
                cmd.env(k, v);
            }
            cmd.stdout(std::process::Stdio::null()).spawn().unwrap()
        })
        .collect();
    // Give them time to start and prepare, then release them together.
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(db.dir.join("go"), b"go").unwrap();
    for mut c in children {
        assert!(c.wait().unwrap().success(), "a child process failed");
    }
    (0..PROCESSES)
        .map(|p| {
            let text = std::fs::read_to_string(db.dir.join(format!("child-{p}.txt"))).unwrap();
            text.lines().map(str::to_string).collect()
        })
        .collect()
}

/// The tx values of a cell's versions in insertion order.
fn insertion_order(db: &SqliteDb, cell: &str) -> Vec<String> {
    let c = db.conn();
    let mut st = c.prepare("SELECT tx FROM versions WHERE cell_id = ?1 ORDER BY rowid").unwrap();
    st.query_map([cell], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn processes_and_threads_keep_the_append_only_guarantees() {
    let db = SqliteDb::migrated();
    {
        let s = db.store_with(SqliteOptions::new().durable(false));
        put_vector_keys(&s, &vector("receipt/stored-number"));
        s.put_descriptor(&fx_rate_descriptor()).unwrap();
        for j in 0..PROCESSES * THREADS * ATTEMPTS {
            s.put_receipt(&version_receipt(j)).unwrap();
        }
    }
    let results = run_children(&db, &[]);
    let all: Vec<&String> = results.iter().flatten().collect();
    // Nothing failed other than as the rules say.
    let errors: Vec<&&String> = all.iter().filter(|l| l.contains(" error:")).collect();
    assert!(errors.is_empty(), "{errors:#?}");

    let cell = Receipt::from_json(&version_receipt(0)).unwrap().cell().id();
    let s = SqliteStore::open(&db.path).unwrap();
    let total = PROCESSES * THREADS * ATTEMPTS;
    for (what, history) in [
        ("version", insertion_order(&db, &cell)),
        ("raw", insertion_order(&db, RAW_CELL)),
        ("name", {
            let c = db.conn();
            let mut st = c.prepare("SELECT tx FROM name_bindings WHERE fn_name = ?1 ORDER BY rowid").unwrap();
            st.query_map([NAME], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        }),
    ] {
        let lines: Vec<&&String> = all.iter().filter(|l| l.starts_with(&format!("{what} "))).collect();
        let attempts = if what == "raw" { PROCESSES * ATTEMPTS * 2 } else { total };
        assert_eq!(lines.len(), attempts, "{what}");
        let mut ok: Vec<String> =
            lines.iter().filter(|l| l.split(' ').nth(1) == Some("ok")).map(|l| l.split(' ').nth(2).unwrap().to_string()).collect();
        let not_later = lines.iter().filter(|l| l.split(' ').nth(1) == Some("notlater")).count();
        assert_eq!(ok.len() + not_later, attempts, "{what}");
        // Inserted in tx order: each success was later than everything before it.
        assert!(history.windows(2).all(|w| w[0] < w[1]), "{what}: not in tx order: {history:?}");
        ok.sort();
        assert_eq!(history, ok, "{what}: the history is exactly the successes");
        assert!(history.len() > 1, "{what}: {history:?}");
        eprintln!("{what}: {} of {attempts} appended, {not_later} not later", history.len());
    }
    // The store's own reads agree.
    let versions = s.versions(&cell, "0001-01-01T00:00:00.000000Z", "9999-12-31T23:59:59.999999Z").unwrap();
    assert_eq!(versions.into_iter().map(|v| v.tx).collect::<Vec<_>>(), insertion_order(&db, &cell));

    // Computed versions: one winner per round, the others conflicts.
    for r in 0..ROUNDS {
        let lines: Vec<&&String> = all.iter().filter(|l| l.starts_with(&format!("computed{r} "))).collect();
        assert_eq!(lines.len(), PROCESSES * THREADS);
        let winners: Vec<&str> =
            lines.iter().filter(|l| l.split(' ').nth(1) == Some("ok")).map(|l| l.split(' ').nth(2).unwrap()).collect();
        assert_eq!(winners.len(), 1, "round {r}: {lines:?}");
        assert!(lines.iter().all(|l| matches!(l.split(' ').nth(1), Some("ok" | "conflict"))), "{lines:?}");
        let at = format!("2024-04-01T{r:02}:00:00.000000Z");
        assert_eq!(s.computed_asof(&cell, &at).unwrap().unwrap().receipt_hash, winners[0]);
    }
    assert_eq!(db.conn().query_row("SELECT count(*) FROM computed_versions", [], |r| r.get::<_, i64>(0)).unwrap(), ROUNDS as i64);
    // Puts of one receipt and one manifest: all ok, stored once.
    for what in ["same_receipt", "manifest", "rival_receipt"] {
        let lines: Vec<&&String> = all.iter().filter(|l| l.starts_with(&format!("{what} "))).collect();
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|l| l.split(' ').nth(1) == Some("ok")), "{what}: {lines:?}");
    }
    let c = db.conn();
    let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(n("SELECT count(*) FROM manifests"), 1);
    assert_eq!(n("SELECT count(*) FROM manifest_members"), 1000);
    let same = Receipt::from_json(&stored_number_at("9.99", "2024-05-01T00:00:00.000000Z")).unwrap();
    assert_eq!(n(&format!("SELECT count(*) FROM receipts WHERE hash = '{}'", same.hash())), 1);
    assert_eq!(
        n("SELECT count(*) FROM receipts"),
        (total + 1 + PROCESSES * THREADS * ROUNDS) as i64,
        "every receipt stored once"
    );
    // The index of cells holds each cell once.
    assert_eq!(n("SELECT count(*) FROM cells"), 1);
    // And the file is sound.
    let check: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(check, "ok");
}

#[test]
fn processes_migrating_one_empty_file_apply_once() {
    let db = SqliteDb::empty();
    let results = run_children(&db, &[("DM_SQLITE_CHILD_MIGRATE", "1")]);
    let applied: Vec<usize> =
        results.iter().map(|lines| lines[0].strip_prefix("migrate ").unwrap().parse().unwrap()).collect();
    assert_eq!(applied.iter().filter(|n| **n > 0).count(), 1, "{applied:?}");
    assert_eq!(applied.iter().copied().max(), Some(migrate::sqlite::MIGRATIONS.len()));
    let s = db.store_with(SqliteOptions::new());
    assert!(migrate::sqlite::check(&s).unwrap().is_empty());
}
