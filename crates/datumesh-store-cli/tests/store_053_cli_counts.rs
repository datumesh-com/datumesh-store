//! Store 0.5.3, SP1 (CONTRACT-round-0.7-P): the CLI import keeps `cell_counts`, on SQLite and
//! on PostgreSQL. `import` stores through `put_receipt`, so the trigger that fills `cells`
//! also adds to the counter; after an import the counter equals a recount of `cells`, a
//! re-import changes nothing, and a second import of the same lines adds no cell.
//! (The CLI writes no function tests: `import` has no test lines, so SP2 needs no CLI test.)

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst))
}

fn run(args: &[&str], url: &str, stdin: Option<&[u8]>) -> Output {
    use std::io::Write;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_datumesh-store"));
    cmd.args(args);
    cmd.env_remove("DATABASE_CA_FILE");
    cmd.env("DATABASE_URL", url);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    {
        let mut si = child.stdin.take().unwrap();
        if let Some(b) = stdin {
            si.write_all(b).unwrap();
        }
    }
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "{args:?}: {:?}\n{}\n{}",
        o.status,
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    o
}

fn fixture() -> Vec<u8> {
    std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export-0.3.0.jsonl")).unwrap()
}

/// (sum of the counter, rows of the counter, cells, distinct (hash, ns, fn) of cells).
type Totals = (i64, i64, i64, i64);

const SQL: [&str; 4] = [
    "SELECT CAST(COALESCE(sum(n), 0) AS BIGINT) FROM cell_counts",
    "SELECT count(*) FROM cell_counts",
    "SELECT count(*) FROM cells",
    "SELECT count(*) FROM (SELECT DISTINCT fn_hash, ns, fn FROM cells) d",
];

fn check(t: Totals) {
    assert!(t.2 > 0, "the import stored cells");
    assert_eq!((t.0, t.1), (t.2, t.3), "the counter equals a recount of cells: {t:?}");
}

#[test]
fn sqlite_import_keeps_cell_counts() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli-053").join(unique("db"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("store.db");
    let url = path.to_str().unwrap().to_string();
    run(&["migrate"], &url, None);
    let totals = || -> Totals {
        let c = rusqlite::Connection::open(&path).unwrap();
        let q = |i: usize| c.query_row(SQL[i], [], |r| r.get::<_, i64>(0)).unwrap();
        (q(0), q(1), q(2), q(3))
    };
    run(&["import", "-"], &url, Some(&fixture()));
    let first = totals();
    check(first);
    run(&["import", "-"], &url, Some(&fixture()));
    assert_eq!(totals(), first, "a re-import adds no cell and no count");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn postgres_import_keeps_cell_counts() {
    use postgres::{Config, NoTls};
    let admin = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let name = unique("dm_cli053");
    let mut c = admin.parse::<Config>().unwrap().connect(NoTls).unwrap();
    c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
    let (base, _) = admin.rsplit_once('/').unwrap();
    let url = format!("{base}/{name}");
    let result = std::panic::catch_unwind(|| {
        run(&["migrate"], &url, None);
        let totals = || -> Totals {
            let mut d = url.parse::<Config>().unwrap().connect(NoTls).unwrap();
            let mut q = |i: usize| d.query_one(SQL[i], &[]).unwrap().get::<_, i64>(0);
            (q(0), q(1), q(2), q(3))
        };
        run(&["import", "-"], &url, Some(&fixture()));
        let first = totals();
        check(first);
        run(&["import", "-"], &url, Some(&fixture()));
        assert_eq!(totals(), first, "a re-import adds no cell and no count");
    });
    let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
