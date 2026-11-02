//! Schema migrations: plain SQL files (`migrations/*.sql`, embedded), applied in order with
//! a tracking table, and a schema check that compares the live database's catalog with the
//! catalog the migrations produce in a scratch database.
//!
//! Each function works through a [`PgStore`], which connects to an empty database without
//! touching it: `migrate::apply(&PgStore::connect(url)?)` creates the schema. The SQLite
//! backend's migrations are [`sqlite`] (since 0.4.0):
//! `migrate::sqlite::apply(&SqliteStore::open(path)?)`.

use postgres::Client;

use crate::error::{Db, Error, Result};
use crate::PgStore;

/// One migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Migration {
    /// Its number; applied in increasing order.
    pub version: u32,
    /// Its file name.
    pub name: &'static str,
    /// Its SQL.
    pub sql: &'static str,
}

impl Migration {
    /// SHA-256 of the SQL, recorded when applied: a migration edited after it was applied is
    /// refused.
    pub fn checksum(&self) -> String {
        datumesh_core::sha256_hex(self.sql.as_bytes())
    }
}

/// Every migration, in order.
pub const MIGRATIONS: &[Migration] = &[
    Migration { version: 1, name: "0001_initial.sql", sql: include_str!("../migrations/0001_initial.sql") },
    Migration {
        version: 2,
        name: "0002_computed_versions.sql",
        sql: include_str!("../migrations/0002_computed_versions.sql"),
    },
    Migration { version: 3, name: "0003_slices.sql", sql: include_str!("../migrations/0003_slices.sql") },
    Migration {
        version: 4,
        name: "0004_receipt_values.sql",
        sql: include_str!("../migrations/0004_receipt_values.sql"),
    },
];

const TRACKING: &str = "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (
    version integer PRIMARY KEY,
    name text NOT NULL,
    checksum text NOT NULL,
    applied_at timestamptz NOT NULL DEFAULT now()
)";

/// Serialises migrators of one database (a session-level advisory lock).
const LOCK: &str = "SELECT pg_advisory_lock(hashtextextended('datumesh-store:migrate', 0))";
const UNLOCK: &str = "SELECT pg_advisory_unlock(hashtextextended('datumesh-store:migrate', 0))";

/// Applies the migrations not yet applied, each in its own transaction, and returns their
/// versions (empty when the schema is current). Concurrent callers are serialised (a
/// session-level advisory lock); a migration whose recorded checksum differs from its SQL is
/// an error. Holds one of the store's pooled connections throughout, so the lock and the
/// tracking table's transactions are on one session.
pub fn apply(store: &PgStore) -> Result<Vec<u32>> {
    let mut conn = store.conn()?;
    apply_client(&mut conn.client)
}

fn apply_client(client: &mut Client) -> Result<Vec<u32>> {
    client.batch_execute(LOCK).db()?;
    let result = apply_locked(client);
    let unlocked = client.batch_execute(UNLOCK).db();
    let applied = result?;
    unlocked?;
    for v in &applied {
        tracing::info!(target: "datumesh_store", version = v, "migration applied");
    }
    Ok(applied)
}

fn apply_locked(client: &mut Client) -> Result<Vec<u32>> {
    client.batch_execute(TRACKING).db()?;
    let rows = client.query("SELECT version, checksum FROM datumesh_schema_migrations", &[]).db()?;
    let mut done = Vec::new();
    for r in &rows {
        let v: i32 = r.try_get(0).db()?;
        let c: String = r.try_get(1).db()?;
        done.push((v, c));
    }
    let mut applied = Vec::new();
    for m in MIGRATIONS {
        let version =
            i32::try_from(m.version).map_err(|_| Error::Migration(format!("{}: version too large", m.name)))?;
        match done.iter().find(|(v, _)| *v == version) {
            Some((_, c)) if *c == m.checksum() => continue,
            Some(_) => {
                return Err(Error::Migration(format!(
                    "{} was applied with another checksum: the file changed after it was applied",
                    m.name
                )))
            }
            None => {}
        }
        let mut t = client.transaction().db()?;
        t.batch_execute(m.sql).db()?;
        t.execute(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
            &[&version, &m.name, &m.checksum()],
        )
        .db()?;
        t.commit().db()?;
        applied.push(m.version);
    }
    if let Some((v, _)) = done.iter().find(|(v, _)| !MIGRATIONS.iter().any(|m| i64::from(m.version) == i64::from(*v))) {
        return Err(Error::Migration(format!("the database has migration {v}, which this version does not know")));
    }
    Ok(applied)
}

/// The catalog of the current schema as sorted lines: relations, columns (position, type,
/// nullability, default, collation, generation), constraints, indexes, triggers (with their
/// enabled state), functions (a hash of their definition), and the recorded migrations.
pub fn catalog(store: &PgStore) -> Result<Vec<String>> {
    catalog_client(&mut store.conn()?.client)
}

fn catalog_client(client: &mut Client) -> Result<Vec<String>> {
    let q = r#"
WITH ns AS (SELECT oid FROM pg_namespace WHERE nspname = current_schema())
SELECT 'relation ' || c.relkind::text || ' ' || c.relname
  FROM pg_class c WHERE c.relnamespace = (SELECT oid FROM ns) AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f')
UNION ALL
SELECT 'column ' || c.relname || '.' || a.attname || ' #' || a.attnum || ' ' || format_type(a.atttypid, a.atttypmod)
       || CASE WHEN a.attnotnull THEN ' not null' ELSE '' END
       || coalesce(' default ' || pg_get_expr(d.adbin, d.adrelid), '')
       || coalesce(' collate ' || co.collname, '')
       || CASE WHEN a.attgenerated::text <> '' THEN ' generated ' || a.attgenerated::text ELSE '' END
       || CASE WHEN a.attidentity::text <> '' THEN ' identity ' || a.attidentity::text ELSE '' END
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
  LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
  LEFT JOIN pg_collation co ON co.oid = a.attcollation AND a.attcollation <> 0
       AND co.oid <> (SELECT oid FROM pg_collation WHERE collname = 'default')
 WHERE c.relnamespace = (SELECT oid FROM ns) AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
   AND a.attnum > 0 AND NOT a.attisdropped
UNION ALL
SELECT 'constraint ' || c.relname || '.' || k.conname || ' ' || pg_get_constraintdef(k.oid)
  FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid
 WHERE k.connamespace = (SELECT oid FROM ns)
UNION ALL
SELECT 'index ' || c.relname || ' ' || pg_get_indexdef(i.indexrelid)
  FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
 WHERE c.relnamespace = (SELECT oid FROM ns)
UNION ALL
SELECT 'trigger ' || c.relname || '.' || t.tgname || ' enabled ' || t.tgenabled::text || ' ' || pg_get_triggerdef(t.oid)
  FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
 WHERE c.relnamespace = (SELECT oid FROM ns) AND NOT t.tgisinternal
UNION ALL
SELECT 'function ' || p.oid::regprocedure::text || ' ' || md5(pg_get_functiondef(p.oid))
  FROM pg_proc p WHERE p.pronamespace = (SELECT oid FROM ns) AND p.prokind IN ('f', 'p')
"#;
    let mut lines = Vec::new();
    for r in client.query(q, &[]).db()? {
        lines.push(r.try_get::<_, String>(0).db()?);
    }
    let tracked: bool =
        client.query_one("SELECT to_regclass('datumesh_schema_migrations') IS NOT NULL", &[]).db()?.try_get(0).db()?;
    if tracked {
        for r in client.query("SELECT version, name, checksum FROM datumesh_schema_migrations", &[]).db()? {
            let (v, n, c): (i32, String, String) = (r.try_get(0).db()?, r.try_get(1).db()?, r.try_get(2).db()?);
            lines.push(format!("migration {v} {n} {c}"));
        }
    }
    lines.sort();
    Ok(lines)
}

/// Compares the live database's schema with what the migrations produce: applies every
/// migration to `scratch` (a store on an empty database the caller creates and drops),
/// then diffs the two catalogs. Returns the differences, `- <line>` for what the live
/// database lacks and `+ <line>` for what it has in addition; empty when they agree.
pub fn check(live: &PgStore, scratch: &PgStore) -> Result<Vec<String>> {
    let mut sc = scratch.conn()?;
    let objects: i64 = sc
        .client
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema())",
            &[],
        ).db()?
        .try_get(0).db()?;
    if objects != 0 {
        return Err(Error::Migration("the scratch database is not empty".into()));
    }
    apply_client(&mut sc.client)?;
    let want = catalog_client(&mut sc.client)?;
    drop(sc);
    let have = catalog(live)?;
    let mut diff: Vec<String> = want.iter().filter(|l| !have.contains(l)).map(|l| format!("- {l}")).collect();
    diff.extend(have.iter().filter(|l| !want.contains(l)).map(|l| format!("+ {l}")));
    Ok(diff)
}

/// SQLite migrations (since 0.4.0): the same contract as the PostgreSQL ones, through a
/// [`SqliteStore`](crate::SqliteStore). `migrations/sqlite/*.sql`, embedded, applied in
/// order with the same tracking table; [`check`] compares the live file's schema with
/// what the migrations produce in a scratch database in memory.
#[cfg(feature = "sqlite")]
pub mod sqlite {
    use rusqlite::Connection;

    use super::Migration;
    use crate::error::{Db, Error, Result};
    use crate::SqliteStore;

    /// Every SQLite migration, in order. One so far: the schema of the PostgreSQL
    /// migrations 1-4 at once (there is no earlier SQLite schema to migrate from).
    pub const MIGRATIONS: &[Migration] = &[Migration {
        version: 1,
        name: "0001_initial.sql",
        sql: include_str!("../migrations/sqlite/0001_initial.sql"),
    }];

    const TRACKING: &str = "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    checksum TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT";

    /// Applies the migrations not yet applied and returns their versions (empty when the
    /// schema is current). All of it is one write transaction (`BEGIN IMMEDIATE`; SQLite's
    /// schema changes are transactional), so concurrent callers, in this process or
    /// others, are serialised by the database's write lock, and a failure leaves nothing
    /// half applied. A migration whose recorded checksum differs from its SQL is an error.
    pub fn apply(store: &SqliteStore) -> Result<Vec<u32>> {
        let c = store.raw()?;
        let applied = apply_conn(&c)?;
        for v in &applied {
            tracing::info!(target: "datumesh_store", version = v, "migration applied");
        }
        Ok(applied)
    }

    fn apply_conn(c: &Connection) -> Result<Vec<u32>> {
        crate::sqlite::write(c, |c| {
            c.execute_batch(TRACKING).db()?;
            let mut done: Vec<(i64, String)> = Vec::new();
            {
                let mut st = c.prepare("SELECT version, checksum FROM datumesh_schema_migrations").db()?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).db()?;
                for r in rows {
                    done.push(r.db()?);
                }
            }
            let mut applied = Vec::new();
            for m in MIGRATIONS {
                match done.iter().find(|(v, _)| *v == i64::from(m.version)) {
                    Some((_, c)) if *c == m.checksum() => continue,
                    Some(_) => {
                        return Err(Error::Migration(format!(
                            "{} was applied with another checksum: the file changed after it was applied",
                            m.name
                        )))
                    }
                    None => {}
                }
                c.execute_batch(m.sql).db()?;
                c.execute(
                    "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES (?1, ?2, ?3)",
                    rusqlite::params![m.version, m.name, m.checksum()],
                )
                .db()?;
                applied.push(m.version);
            }
            if let Some((v, _)) = done.iter().find(|(v, _)| !MIGRATIONS.iter().any(|m| i64::from(m.version) == *v)) {
                return Err(Error::Migration(format!(
                    "the database has migration {v}, which this version does not know"
                )));
            }
            Ok(applied)
        })
    }

    /// The schema as sorted lines: each table, index and trigger with its SQL (whitespace
    /// collapsed), the indexes SQLite makes for keys, and the recorded migrations.
    pub fn catalog(store: &SqliteStore) -> Result<Vec<String>> {
        catalog_conn(&*store.raw()?)
    }

    fn catalog_conn(c: &Connection) -> Result<Vec<String>> {
        let mut lines = Vec::new();
        {
            let mut st = c
                .prepare(
                    "SELECT type, name, tbl_name, sql FROM sqlite_schema
                      WHERE name NOT LIKE 'sqlite_stat%' ORDER BY type, name",
                )
                .db()?;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })
                .db()?;
            for r in rows {
                let (ty, name, table, sql) = r.db()?;
                let sql = sql.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
                lines.push(format!("{ty} {name} on {table}: {sql}"));
            }
        }
        let tracked: bool = c
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'datumesh_schema_migrations')",
                [],
                |r| r.get(0),
            )
            .db()?;
        if tracked {
            let mut st = c.prepare("SELECT version, name, checksum FROM datumesh_schema_migrations").db()?;
            let rows =
                st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))).db()?;
            for r in rows {
                let (v, n, c) = r.db()?;
                lines.push(format!("migration {v} {n} {c}"));
            }
        }
        lines.sort();
        Ok(lines)
    }

    /// [`check`] of the file at `path`, opened with
    /// [`SqliteStore::open_existing`]: a path with no file is an error (`no such database
    /// file`) and nothing is created. Since 0.4.0.
    pub fn check_file(path: impl AsRef<std::path::Path>) -> Result<Vec<String>> {
        check(&SqliteStore::open_existing(path)?)
    }

    /// Compares the live file's schema with what the migrations produce (a store from
    /// [`SqliteStore::open_existing`] checks without creating or changing anything;
    /// [`SqliteStore::open`] creates a missing file): applies every
    /// migration to a scratch database in memory, then diffs the two catalogs. Returns the
    /// differences, `- <line>` for what the live database lacks and `+ <line>` for what it
    /// has in addition; empty when they agree.
    pub fn check(live: &SqliteStore) -> Result<Vec<String>> {
        let scratch = Connection::open_in_memory().db()?;
        apply_conn(&scratch)?;
        let mut want = catalog_conn(&scratch)?;
        let mut have = catalog(live)?;
        want.sort();
        have.sort();
        let mut diff: Vec<String> = want.iter().filter(|l| !have.contains(l)).map(|l| format!("- {l}")).collect();
        diff.extend(have.iter().filter(|l| !want.contains(l)).map(|l| format!("+ {l}")));
        Ok(diff)
    }
}
