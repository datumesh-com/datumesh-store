//! Migration 10 (`0010_function_tests.sql`) and SQLite migration 6 (`sqlite/0006_function_tests.sql`,
//! 0.5.1): the function tests, the descriptor inputs index and the index of the bindings by
//! hash. A store of 0.5.0 gets the inputs index filled from the descriptors it already holds
//! (both backends: from the stored JSON in SQL), and nothing else changes. Both backends.

#[macro_use]
mod common;

use common::*;
use datumesh_store::{migrate, SqliteOptions};

const NAME: &str = "rates.example.org/fx_rate";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn the_inputs_of_descriptors_stored_before_0_5_1_are_indexed(b: Backend) {
    let db = TestDb::empty(b);
    let (old, all): (Vec<&migrate::Migration>, usize) = match b {
        Backend::Pg => (migrate::MIGRATIONS[..9].iter().collect(), migrate::MIGRATIONS.len()),
        Backend::Sqlite => (migrate::sqlite::MIGRATIONS[..5].iter().collect(), migrate::sqlite::MIGRATIONS.len()),
    };
    assert_eq!(
        all,
        old.len() + 5,
        "five migrations more than 0.5.0 on each backend (0.5.1, 0.5.2, 0.5.3, facets, withdrawals)"
    );
    db.exec(db.pick(
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version integer PRIMARY KEY, name text NOT NULL, checksum text NOT NULL, applied_at timestamptz NOT NULL DEFAULT now())",
        "CREATE TABLE IF NOT EXISTS datumesh_schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum TEXT NOT NULL, applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))) STRICT",
    ))
    .unwrap();
    for m in &old {
        db.exec(m.sql).unwrap();
        db.exec(&format!(
            "INSERT INTO datumesh_schema_migrations (version, name, checksum) VALUES ({}, '{}', '{}')",
            m.version,
            m.name,
            m.checksum()
        ))
        .unwrap();
    }
    // Two descriptors as 0.5.0 wrote them: one with inputs, one without.
    let with = jcs(format!(
        r#"{{"code":{{"body":"convert","lang":"stored"}},"inputs":["{NAME}"],"signature":{{"args":{{"base":{{"type":"string","valid":false}}}},"returns":{{"type":"number","unit":"1"}}}}}}"#
    )
    .as_bytes());
    let plain = fx_rate_descriptor();
    let (hw, hp) = (fn_hash_of(&with), fn_hash_of(&plain));
    for (h, bytes) in [(&hw, &with), (&hp, &plain)] {
        db.exec(db.pick(
            &format!("INSERT INTO descriptors (fn_hash, bytes) VALUES ('{h}', decode('{}', 'hex'))", hex(bytes)),
            &format!("INSERT INTO descriptors (fn_hash, bytes) VALUES ('{h}', X'{}')", hex(bytes)),
        ))
        .unwrap();
    }

    let applied = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert_eq!(
        applied,
        [all as u32 - 4, all as u32 - 3, all as u32 - 2, all as u32 - 1, all as u32],
        "only the 0.5.1, 0.5.2, 0.5.3, facets and withdrawals migrations are applied"
    );

    let s = db.bare_store();
    assert_eq!(s.function_readers(NAME).unwrap(), vec![hw.clone()]);
    assert_eq!(db.count("descriptor_inputs"), 1);
    assert_eq!(db.count("function_tests"), 0);
    // A descriptor stored after the migration is indexed by put_descriptor.
    let h2 = s.put_descriptor(&with_inputs_of("convert v2", NAME)).unwrap();
    let mut both = vec![hw.clone(), h2];
    both.sort();
    assert_eq!(s.function_readers(NAME).unwrap(), both);

    // Re-running is a no-op.
    let again = match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    };
    assert!(again.is_empty());
}

/// A canonical descriptor of `body` with one input.
fn with_inputs_of(body: &str, input: &str) -> Vec<u8> {
    jcs(format!(
        r#"{{"code":{{"body":"{body}","lang":"stored"}},"inputs":["{input}"],"signature":{{"args":{{"base":{{"type":"string","valid":false}}}},"returns":{{"type":"number","unit":"1"}}}}}}"#
    )
    .as_bytes())
}

backends!(the_inputs_of_descriptors_stored_before_0_5_1_are_indexed);
