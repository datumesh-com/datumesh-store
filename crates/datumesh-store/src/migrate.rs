//! Schema migrations: plain SQL files (`migrations/*.sql`, embedded), applied in order with
//! a tracking table, and a schema check that compares the live database's catalog with the
//! catalog the migrations produce in a scratch database.

use postgres::Client;

use crate::error::{Error, Result};

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
pub const MIGRATIONS: &[Migration] =
    &[Migration { version: 1, name: "0001_initial.sql", sql: include_str!("../migrations/0001_initial.sql") }];

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
/// versions (empty when the schema is current). Concurrent callers are serialised; a
/// migration whose recorded checksum differs from its SQL is an error.
pub fn apply(client: &mut Client) -> Result<Vec<u32>> {
    client.batch_execute(LOCK)?;
    let result = apply_locked(client);
    let unlocked = client.batch_execute(UNLOCK);
    let applied = result?;
    unlocked?;
    for v in &applied {
        tracing::info!(target: "datumesh_store", version = v, "migration applied");
    }
    Ok(applied)
}

fn apply_locked(client: &mut Client) -> Result<Vec<u32>> {
    client.batch_execute(TRACKING)?;
    let rows = client.query("SELECT version, checksum FROM datumesh_schema_migrations", &[])?;
    let mut done = Vec::new();
    for r in &rows {
        let v: i32 = r.try_get(0)?;
        let c: String = r.try_get(1)?;
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
        let mut t = client.transaction()?;
        t.batch_execute(m.sql)?;
        t.execute(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ($1, $2, $3)",
            &[&version, &m.name, &m.checksum()],
        )?;
        t.commit()?;
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
pub fn catalog(client: &mut Client) -> Result<Vec<String>> {
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
    for r in client.query(q, &[])? {
        lines.push(r.try_get::<_, String>(0)?);
    }
    let tracked: bool =
        client.query_one("SELECT to_regclass('datumesh_schema_migrations') IS NOT NULL", &[])?.try_get(0)?;
    if tracked {
        for r in client.query("SELECT version, name, checksum FROM datumesh_schema_migrations", &[])? {
            let (v, n, c): (i32, String, String) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
            lines.push(format!("migration {v} {n} {c}"));
        }
    }
    lines.sort();
    Ok(lines)
}

/// Compares the live database's schema with what the migrations produce: applies every
/// migration to `scratch` (which must be an empty database the caller creates and drops),
/// then diffs the two catalogs. Returns the differences, `- <line>` for what the live
/// database lacks and `+ <line>` for what it has in addition; empty when they agree.
pub fn check(live: &mut Client, scratch: &mut Client) -> Result<Vec<String>> {
    let objects: i64 = scratch
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = current_schema())",
            &[],
        )?
        .try_get(0)?;
    if objects != 0 {
        return Err(Error::Migration("the scratch database is not empty".into()));
    }
    apply(scratch)?;
    let want = catalog(scratch)?;
    let have = catalog(live)?;
    let mut diff: Vec<String> = want.iter().filter(|l| !have.contains(l)).map(|l| format!("- {l}")).collect();
    diff.extend(have.iter().filter(|l| !want.contains(l)).map(|l| format!("+ {l}")));
    Ok(diff)
}
