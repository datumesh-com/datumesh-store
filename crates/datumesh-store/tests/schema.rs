//! The schema: every SQL statement the store uses prepares against the migrated schema
//! (there is no compile-time query checking with either driver); migrations, through a
//! store on an empty database, apply from empty, re-run as a no-op, apply once when
//! concurrent, and `check` finds no drift on a migrated database but reports drift
//! introduced by hand (both backends: `migrate` for PostgreSQL, `migrate::sqlite` for
//! SQLite); instants round-trip through `timestamptz` exactly (PostgreSQL) and order as
//! text (both).

#[macro_use]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use datumesh_store::{migrate, PgStore, PoolOptions, Resolver, SqliteOptions, SqliteStore};

/// `check_statements` of a new store of `db`.
fn check_statements(db: &TestDb) -> datumesh_store::Result<usize> {
    match db.backend {
        Backend::Pg => db.pg().bare_store().check_statements(),
        Backend::Sqlite => db.lite().store_with(SqliteOptions::new()).check_statements(),
    }
}

/// The versions of the backend's migrations.
fn versions(b: Backend) -> Vec<u32> {
    match b {
        Backend::Pg => migrate::MIGRATIONS.iter().map(|m| m.version).collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS.iter().map(|m| m.version).collect(),
    }
}

/// `apply` through a new store of `db`.
fn apply(db: &TestDb) -> datumesh_store::Result<Vec<u32>> {
    match db.backend {
        Backend::Pg => migrate::apply(&db.pg().bare_store()),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())),
    }
}

/// `catalog` through a new store of `db`.
fn catalog(db: &TestDb) -> Vec<String> {
    match db.backend {
        Backend::Pg => migrate::catalog(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::catalog(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    }
}

/// `check` of `db` (PostgreSQL: against a new empty scratch database; SQLite: the check
/// makes its own, in memory).
fn check(db: &TestDb) -> Vec<String> {
    match db.backend {
        Backend::Pg => {
            let scratch = PgDb::empty();
            migrate::check(&db.pg().bare_store(), &scratch.bare_store()).unwrap()
        }
        Backend::Sqlite => migrate::sqlite::check(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    }
}

fn every_statement_prepares_against_the_migrated_schema(b: Backend) {
    let db = TestDb::migrated(b);
    let n = check_statements(&db).unwrap();
    assert!(n >= 20, "{n}");
}

fn statements_do_not_prepare_without_the_schema(b: Backend) {
    let db = TestDb::empty(b);
    assert!(check_statements(&db).is_err());
}

fn migrations_apply_from_empty_and_rerun_as_no_op(b: Backend) {
    let db = TestDb::empty(b);
    let applied = apply(&db).unwrap();
    let all = versions(b);
    assert_eq!(applied, all);
    assert!(!all.is_empty());
    let before = catalog(&db);
    // Again, through the same and through a new store: nothing to do.
    assert!(apply(&db).unwrap().is_empty());
    assert!(apply(&db).unwrap().is_empty());
    assert_eq!(catalog(&db), before);
    assert_eq!(db.count("datumesh_schema_migrations") as usize, all.len());
}

fn a_store_on_an_empty_database_migrates_it_and_then_works(b: Backend) {
    let db = TestDb::empty(b);
    // Connecting does not touch the database.
    assert!(catalog(&db).is_empty());
    let objects = db.int(db.pick(
        "SELECT count(*) FROM pg_class WHERE relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = 'public')",
        "SELECT count(*) FROM sqlite_schema",
    ));
    assert_eq!(objects, 0);
    assert!(!apply(&db).unwrap().is_empty());
    let s = db.bare_store();
    put_vector_keys(&s, &vector("receipt/stored-number"));
    let h = s.put_receipt(&input(&vector("receipt/stored-number"))).unwrap();
    assert!(s.receipt(&h).is_some());
    assert!(check_statements(&db).unwrap() >= 20);
}

/// PostgreSQL: `apply` holds one pooled connection throughout (a pool of one suffices) and
/// releases the advisory lock. SQLite: `apply` is one write transaction, and the write lock
/// is free afterwards.
fn apply_holds_one_connection_and_releases_the_lock(b: Backend) {
    let db = TestDb::empty(b);
    match b {
        Backend::Pg => {
            let s =
                db.pg().bare_store_with(PoolOptions::new().max_size(1).checkout_timeout(Duration::from_millis(500)));
            assert!(!migrate::apply(&s).unwrap().is_empty());
            assert!(migrate::apply(&s).unwrap().is_empty());
            assert!(!migrate::catalog(&s).unwrap().is_empty());
            let free: bool = db
                .pg()
                .client()
                .query_one("SELECT pg_try_advisory_lock(hashtextextended('datumesh-store:migrate', 0))", &[])
                .unwrap()
                .get(0);
            assert!(free);
        }
        Backend::Sqlite => {
            let s = db.lite().store_with(SqliteOptions::new().max_size(1));
            assert!(!migrate::sqlite::apply(&s).unwrap().is_empty());
            assert!(migrate::sqlite::apply(&s).unwrap().is_empty());
            let c = db.lite().conn();
            c.busy_timeout(Duration::ZERO).unwrap();
            c.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        }
    }
}

/// Six concurrent migrators on an empty database, each through a store of its own (half
/// opened by URL or path): exactly one applies the migrations.
fn concurrent_migrations_apply_once(b: Backend) {
    let db = Arc::new(TestDb::empty(b));
    let mut handles = Vec::new();
    for i in 0..6 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            if i % 2 == 0 {
                apply(&db).unwrap()
            } else {
                match db.backend {
                    Backend::Pg => migrate::apply(&PgStore::connect(&db.url()).unwrap()).unwrap(),
                    Backend::Sqlite => migrate::sqlite::apply(&SqliteStore::open(db.url()).unwrap()).unwrap(),
                }
            }
        }));
    }
    let applied: Vec<Vec<u32>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(applied.iter().filter(|a| !a.is_empty()).count(), 1, "{applied:?}");
}

fn check_is_clean_after_migrating_and_finds_drift(b: Backend) {
    let live = TestDb::migrated(b);
    let diff = check(&live);
    assert!(diff.is_empty(), "{diff:#?}");

    // Drift: each change alone is found.
    let cases: &[(&str, &str)] = match b {
        Backend::Pg => &[
            ("ALTER TABLE receipts ADD COLUMN note text", "note"),
            ("DROP INDEX edges_input_hash", "edges_input_hash"),
            ("ALTER TABLE versions DISABLE TRIGGER versions_append_only", "versions_append_only"),
            ("ALTER TABLE blobs ALTER COLUMN media_type SET DEFAULT 'application/octet-stream'", "media_type"),
            ("CREATE TABLE extra (x int)", "extra"),
        ],
        Backend::Sqlite => &[
            ("ALTER TABLE receipts ADD COLUMN note TEXT", "note"),
            ("DROP INDEX edges_input_hash", "edges_input_hash"),
            ("DROP TRIGGER versions_no_update", "versions_no_update"),
            ("DROP TRIGGER versions_in_order", "versions_in_order"),
            ("CREATE TABLE extra (x INTEGER)", "extra"),
            ("CREATE INDEX extra_index ON blobs (length)", "extra_index"),
        ],
    };
    for (sql, needle) in cases {
        let live = TestDb::migrated(b);
        live.exec(sql).unwrap();
        let diff = check(&live);
        assert!(!diff.is_empty(), "{sql}: no drift found");
        assert!(diff.iter().any(|l| l.contains(needle)), "{sql}: {diff:#?}");
    }
}

/// PostgreSQL only: the scratch database is the caller's, and must be empty (SQLite's
/// check makes its own in memory).
#[test]
fn check_refuses_a_scratch_database_that_is_not_empty() {
    let live = PgDb::migrated();
    let scratch = PgDb::migrated();
    assert!(migrate::check(&live.bare_store(), &scratch.bare_store()).is_err());
}

fn an_edited_migration_is_refused(b: Backend) {
    let db = TestDb::migrated(b);
    db.exec("UPDATE datumesh_schema_migrations SET checksum = 'x' WHERE version = 1").unwrap();
    let e = apply(&db).unwrap_err();
    assert!(e.to_string().contains("checksum"), "{e}");
}

/// SQLite: instants are TEXT with the default (BINARY) collation, so they order as bytes;
/// fixed width makes that chronological. Random instants of the specification's range,
/// ordered by the database, are in the order of their (day, microsecond) values.
#[test]
fn instants_order_chronologically_as_text_in_sqlite() {
    let db = TestDb::migrated(Backend::Sqlite);
    let c = db.lite().conn();
    c.execute_batch("CREATE TEMP TABLE t (i TEXT NOT NULL)").unwrap();
    let mut state = 0x243f6a8885a308d3u64;
    let mut next = || {
        state = state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };
    let mut want: Vec<(i64, u64, String)> = Vec::new();
    for _ in 0..20_000 {
        let n = 1 + (next() % 3_652_059) as i64;
        let (y, m, d) = civil(n);
        let us = next() % 86_400_000_000;
        let (s, frac) = (us / 1_000_000, us % 1_000_000);
        let text = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z", s / 3600, (s / 60) % 60, s % 60);
        c.execute("INSERT INTO t VALUES (?1)", [&text]).unwrap();
        want.push((n, us, text));
    }
    want.sort();
    let got: Vec<String> = c
        .prepare("SELECT i FROM t ORDER BY i")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(got, want.into_iter().map(|(_, _, t)| t).collect::<Vec<_>>());
    // And a comparison in SQL agrees: BETWEEN on instants is chronological.
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM t WHERE i BETWEEN '1582-10-15T00:00:00.000000Z' AND '1970-01-01T00:00:00.000000Z'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(n > 0);
}

/// Every instant of the specification's range round-trips through `timestamptz` exactly,
/// whatever the session's TimeZone and DateStyle, and `timestamptz` order equals the text
/// order under `COLLATE "C"`.
#[test]
fn instants_round_trip_through_timestamptz() {
    let db = PgDb::migrated();
    let mut c = db.client();
    // Days from 0001-01-01 to 9999-12-31 inclusive: 3652059. Sample deterministically
    // (splitmix64), plus edges: first and last instants, leap days, century years, the
    // Julian-Gregorian switch, the Unix epoch, and the ends of each day.
    let mut instants = vec![
        "0001-01-01T00:00:00.000000Z".to_string(),
        "9999-12-31T23:59:59.999999Z".to_string(),
        "0004-02-29T12:00:00.000001Z".to_string(),
        "1582-10-04T23:59:59.999999Z".to_string(),
        "1582-10-15T00:00:00.000000Z".to_string(),
        "1600-02-29T00:00:00.000000Z".to_string(),
        "1900-02-28T23:59:59.999999Z".to_string(),
        "1969-12-31T23:59:59.999999Z".to_string(),
        "1970-01-01T00:00:00.000000Z".to_string(),
        "2000-02-29T00:00:00.000000Z".to_string(),
        "2038-01-19T03:14:08.000000Z".to_string(),
        "0999-12-31T23:59:59.999999Z".to_string(),
        "1000-01-01T00:00:00.000000Z".to_string(),
    ];
    let mut state = 0x9e3779b97f4a7c15u64;
    let mut next = || {
        state = state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };
    for _ in 0..20_000 {
        let (y, m, d) = civil(1 + (next() % 3_652_059) as i64);
        let us = next() % 86_400_000_000;
        let (s, frac) = (us / 1_000_000, us % 1_000_000);
        instants.push(format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z", s / 3600, (s / 60) % 60, s % 60));
    }
    for (tz, style) in [
        ("UTC", "ISO, YMD"),
        ("Asia/Kathmandu", "SQL, DMY"),
        ("America/St_Johns", "German, DMY"),
        ("Pacific/Kiritimati", "Postgres, MDY"),
        ("Europe/Amsterdam", "ISO, MDY"),
    ] {
        c.batch_execute(&format!("SET TIME ZONE '{tz}'; SET DateStyle = '{style}'")).unwrap();
        let rows = c
            .query(
                "SELECT t, to_char(t::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')
                 FROM unnest($1::text[]) AS t",
                &[&instants],
            )
            .unwrap();
        for row in &rows {
            let (a, b): (String, String) = (row.get(0), row.get(1));
            assert_eq!(a, b, "{tz} {style}");
        }
        let bad: i64 = c
            .query_one(
                "SELECT count(*) FROM (SELECT row_number() OVER (ORDER BY t COLLATE \"C\") a,
                        row_number() OVER (ORDER BY t::timestamptz) b FROM unnest($1::text[]) AS t) x WHERE a <> b",
                &[&instants],
            )
            .unwrap()
            .get(0);
        assert_eq!(bad, 0, "{tz} {style}");
    }
}

/// The proleptic Gregorian date of day `n` (1 = 0001-01-01).
fn civil(n: i64) -> (i64, i64, i64) {
    // Howard Hinnant's days_from_civil inverse, with day 1 = 0001-01-01 = -719162 from 1970.
    let z = n - 1 - 719_162 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[test]
fn civil_dates_are_right() {
    assert_eq!(civil(1), (1, 1, 1));
    assert_eq!(civil(719_163), (1970, 1, 1));
    assert_eq!(civil(3_652_059), (9999, 12, 31));
}

backends!(
    every_statement_prepares_against_the_migrated_schema,
    statements_do_not_prepare_without_the_schema,
    migrations_apply_from_empty_and_rerun_as_no_op,
    a_store_on_an_empty_database_migrates_it_and_then_works,
    apply_holds_one_connection_and_releases_the_lock,
    concurrent_migrations_apply_once,
    check_is_clean_after_migrating_and_finds_drift,
    an_edited_migration_is_refused,
);
