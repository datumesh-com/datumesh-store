//! Store 0.6.5, the guard patch G2 and G7 (CONTRACT-guard-patch.md): one small row per member set,
//! `member_sets`, migration PostgreSQL 16 (`0016_member_sets.sql`), SQLite 12 (`0012_member_sets.sql`).
//! Both backends. Failing tests: the table, the type and the methods are not written yet.
//!
//! API assumed (the contract names the table, its columns and the two methods; the Rust shapes are
//! the test writer's choice, stated here for the implementer):
//!
//! ```text
//! /// A member-set row (G2): what a slice read saw, without its members.
//! #[non_exhaustive]
//! #[derive(Clone, Debug, PartialEq, Eq)]
//! pub struct MemberSet {
//!     pub entry: String,         // "slice:<slice_id>:<members_hash>"
//!     pub slice_id: String,
//!     pub members_hash: String,
//!     pub fn_hash: String,       // the slice's function hash
//!     pub slice: Vec<u8>,        // the slice object's canonical bytes (Slice::canonical)
//!     pub members: u64,          // how many members
//!     pub tx: Option<String>,    // the latest member or withdrawal tx the read saw (SliceRead.tx); None: none
//!     pub asof: String,          // the read's as-of
//!     pub licences: Vec<String>, // the members' distinct licences, sorted (decided 26 Nov, G3)
//! }
//! impl MemberSet {
//!     /// The row of `manifest` (entry, ids, fn_hash, slice bytes and count from it).
//!     pub fn from_manifest(manifest: &datumesh_core::Manifest, tx: Option<&str>, asof: &str) -> MemberSet;
//! }
//! // on Store (re-exported at the crate root as datumesh_store::MemberSet):
//! /// Inserts the row, or nothing when a row with its entry is held. Ok(true) when inserted.
//! /// Refuses a row whose entry is not 'slice:'||slice_id||':'||members_hash, whose slice bytes are
//! /// not a canonical slice object of that id and function hash, or a malformed instant (Error::Invalid).
//! fn put_member_set(&self, set: &MemberSet) -> Result<bool>;
//! /// The row of `entry`, or None.
//! fn member_set(&self, entry: &str) -> Result<Option<MemberSet>>;
//! ```
//!
//! The `tx` column is nullable: an empty slice with no withdrawal has no member tx (SliceRead.tx is
//! None). The table is read with raw SQL through the columns the contract names: `entry`,
//! `slice_id`, `members_hash`, `fn_hash`, `slice`, `members`, `tx`, `asof`, and `licences` (the
//! members' distinct licence URLs, sorted, as JCS text: "the member-set row carries its members'
//! licences", decided 26 Nov, in 0016 / 0012). `from_manifest` leaves `licences` empty (a manifest
//! has no licences); the caller sets the field. The store keeps the licences distinct and sorted
//! whatever order it is given them in.
//!
//! The migration tests apply 1..=15 (SQLite 1..=11), write as store 0.6.4 does (a manifest, its
//! members, a receipt citing it), then apply the rest: only 16 (12) is applied, and the manifest
//! tables are untouched (G7).

#[macro_use]
mod common;

use common::*;
use datumesh_core::{Manifest, Receipt, Resolver, Slice, Type};
use datumesh_store::{migrate, MemberSet, SqliteOptions};

const T1: &str = "2024-01-01T00:00:00.000000Z";
const T2: &str = "2024-01-02T00:00:00.000000Z";
const T3: &str = "2024-01-03T00:00:00.000000Z";

fn h_of(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

/// `f(k)` of SLICE_HOST, stored; returns its function hash.
fn function(s: &TStore) -> String {
    s.put_descriptor(&stored_descriptor(&[("k", Type::Number)], "f")).unwrap()
}

fn slice_f(fn_hash: &str) -> Slice {
    slice(&format!(r#"{{"args":{{"k":"*"}},"fn":"f","fn_hash":"{fn_hash}","ns":"{SLICE_HOST}"}}"#))
}

fn manifest(fn_hash: &str, members: &[String]) -> Manifest {
    let mut m = members.to_vec();
    m.sort();
    m.dedup();
    Manifest::new(slice_f(fn_hash), m).unwrap()
}

fn kcell(fn_hash: &str, k: u32) -> datumesh_core::Cell {
    slice_cell("f", fn_hash, &[("k", sc(Type::Number, &k.to_string()))])
}

fn columns(db: &TestDb, table: &str) -> Vec<String> {
    let pg = format!(
        "SELECT column_name::text FROM information_schema.columns WHERE table_name = '{table}' AND table_schema = current_schema() ORDER BY column_name"
    );
    let lite = format!("SELECT name FROM pragma_table_info('{table}') ORDER BY name");
    let rows = db.texts(db.pick(&pg, &lite));
    rows.into_iter().map(|r| r[0].clone()).collect()
}

/// Every row of a table as text (`bytes`/`slice` blobs as hex), ordered, for before/after comparisons.
fn dump(db: &TestDb, sql_pg: &str, sql_lite: &str) -> Vec<Vec<String>> {
    db.texts(db.pick(sql_pg, sql_lite))
}

// ---- G2: the table ---------------------------------------------------------------------------

fn the_table_has_the_columns_the_contract_names(b: Backend) {
    let db = TestDb::migrated(b);
    let c = columns(&db, "member_sets");
    for want in ["asof", "entry", "fn_hash", "licences", "members", "members_hash", "slice", "slice_id", "tx"] {
        assert!(c.contains(&want.to_string()), "member_sets has {want}: {c:?}");
    }
    assert_eq!(db.count("member_sets"), 0, "a fresh store holds no row");
}

fn put_member_set_writes_one_row_and_member_set_reads_it(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m1 = put_member(&s, &kcell(&h, 1), "10", T1, T1);
    let m2 = put_member(&s, &kcell(&h, 2), "20", T2, T2);
    let m = manifest(&h, &[m1.clone(), m2.clone()]);
    let set = MemberSet::from_manifest(&m, Some(T2), T3);
    // What the row holds, from the manifest.
    assert_eq!(set.entry, m.entry());
    assert_eq!(set.slice_id, m.slice().id());
    assert_eq!(set.members_hash, m.members_hash());
    assert_eq!(set.fn_hash, h);
    assert_eq!(set.slice, m.slice().canonical());
    assert_eq!(set.members, 2);
    assert_eq!(set.tx.as_deref(), Some(T2));
    assert_eq!(set.asof, T3);

    assert_eq!(s.member_set(&m.entry()).unwrap(), None, "nothing before the put");
    assert!(s.put_member_set(&set).unwrap(), "inserted");
    assert_eq!(s.member_set(&m.entry()).unwrap().as_ref(), Some(&set), "read back as written");
    assert_eq!(db.count("member_sets"), 1);
    // The raw row: the columns the contract names.
    let row = &dump(
        &db,
        "SELECT entry, slice_id, members_hash, fn_hash, encode(slice, 'escape'), members::text, tx, asof FROM member_sets",
        "SELECT entry, slice_id, members_hash, fn_hash, CAST(slice AS TEXT), CAST(members AS TEXT), tx, asof FROM member_sets",
    )[0];
    assert_eq!(row[0], m.entry());
    assert_eq!(row[1], m.slice().id());
    assert_eq!(row[2], m.members_hash());
    assert_eq!(row[3], h);
    assert_eq!(row[4].as_bytes(), m.slice().canonical().as_slice(), "the slice object's canonical bytes");
    assert_eq!((row[5].as_str(), row[6].as_str(), row[7].as_str()), ("2", T2, T3));
    // A member set is not a manifest: nothing is written to the manifest tables.
    assert_eq!(db.count("manifests"), 0);
    assert_eq!(db.count("manifest_members"), 0);
    assert_eq!(s.manifest(&m.entry()), None, "no manifest is served from a member-set row by the store");
}

fn the_row_carries_its_members_licences_distinct_and_sorted(b: Backend) {
    // Decided 26 Nov (G3): the reuse check takes the licences from the row and reads no member.
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let restricted = "https://rates.example.org/licences/restricted";
    let m = manifest(&h, &[h_of('a'), h_of('b'), h_of('c')]);
    let mut set = MemberSet::from_manifest(&m, Some(T1), T2);
    assert!(set.licences.is_empty(), "a manifest has no licences: the caller sets them");
    set.licences = vec![restricted.to_string(), "MIT".to_string(), restricted.to_string(), "CC0-1.0".to_string()];
    assert!(s.put_member_set(&set).unwrap());
    let back = s.member_set(&m.entry()).unwrap().expect("the row");
    assert_eq!(back.licences, ["CC0-1.0", "MIT", restricted], "distinct, sorted");
    // The column: the JCS text of the sorted list.
    let raw = db.texts("SELECT licences FROM member_sets");
    assert_eq!(raw[0][0], format!(r#"["CC0-1.0","MIT","{restricted}"]"#));
    // An empty slice: no licences, an empty list.
    let empty = manifest(&h, &[]);
    assert!(s.put_member_set(&MemberSet::from_manifest(&empty, None, T2)).unwrap());
    assert_eq!(s.member_set(&empty.entry()).unwrap().unwrap().licences, Vec::<String>::new());
    assert_eq!(db.texts(&format!("SELECT licences FROM member_sets WHERE entry = '{}'", empty.entry()))[0][0], "[]");
    // A licence that is not a string of the right shape is refused by the store (empty text).
    let other = manifest(&h, &[h_of('d')]);
    let mut bad = MemberSet::from_manifest(&other, Some(T1), T2);
    bad.licences = vec![String::new()];
    assert!(matches!(s.put_member_set(&bad), Err(datumesh_store::Error::Invalid(_))), "an empty licence");
    assert_eq!(db.count("member_sets"), 2);
}

fn put_member_set_is_insert_or_nothing(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m = manifest(&h, &[h_of('a'), h_of('b')]);
    let first = MemberSet::from_manifest(&m, Some(T1), T2);
    assert!(s.put_member_set(&first).unwrap());
    // The same row again: nothing, no error.
    assert!(!s.put_member_set(&first).unwrap(), "held already");
    // Another read of the same member set, later: nothing either; the first row is kept.
    let later = MemberSet::from_manifest(&m, Some(T2), T3);
    assert!(!s.put_member_set(&later).unwrap());
    assert_eq!(s.member_set(&m.entry()).unwrap().as_ref(), Some(&first), "the first row is kept");
    assert_eq!(db.count("member_sets"), 1);
    // Another member set of the same slice: a row of its own.
    let other = manifest(&h, &[h_of('a')]);
    assert!(s.put_member_set(&MemberSet::from_manifest(&other, Some(T1), T3)).unwrap());
    assert_eq!(db.count("member_sets"), 2);
    // Concurrent writers of one row: one insert, the others nothing.
    let third = manifest(&h, &[h_of('c')]);
    let set = MemberSet::from_manifest(&third, Some(T1), T3);
    let inserted: usize = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let st = db.open_again();
                let set = set.clone();
                sc.spawn(move || st.put_member_set(&set).unwrap() as usize)
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    assert_eq!(inserted, 1, "exactly one writer inserts");
    assert_eq!(db.count("member_sets"), 3);
}

fn an_empty_slice_has_no_tx(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m = manifest(&h, &[]);
    let set = MemberSet::from_manifest(&m, None, T2);
    assert_eq!((set.members, set.tx.as_deref()), (0, None));
    assert!(s.put_member_set(&set).unwrap());
    assert_eq!(s.member_set(&m.entry()).unwrap(), Some(set));
}

fn a_member_set_and_a_manifest_of_one_entry_coexist(b: Backend) {
    // Under the Store policy both are written (G2): neither refuses the other.
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m = manifest(&h, &[h_of('a'), h_of('b')]);
    assert_eq!(s.put_manifest(&m.canonical()).unwrap(), m.entry());
    assert!(s.put_member_set(&MemberSet::from_manifest(&m, Some(T1), T2)).unwrap());
    assert_eq!(s.manifest(&m.entry()).unwrap().as_ref(), m.canonical().as_slice());
    assert!(s.member_set(&m.entry()).unwrap().is_some());
    assert_eq!((db.count("manifests"), db.count("manifest_members"), db.count("member_sets")), (1, 2, 1));
}

fn malformed_rows_are_refused_by_the_store(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m = manifest(&h, &[h_of('a')]);
    let good = MemberSet::from_manifest(&m, Some(T1), T2);
    let invalid = |set: &MemberSet, what: &str| match s.put_member_set(set) {
        Err(datumesh_store::Error::Invalid(_)) => {}
        other => panic!("{what}: {other:?}"),
    };
    let mut x = good.clone();
    x.entry = format!("slice:{}:{}", good.slice_id, h_of('f'));
    invalid(&x, "the entry is not slice:<slice_id>:<members_hash>");
    let mut x = good.clone();
    x.members_hash = h_of('f');
    invalid(&x, "the members hash is not the entry's");
    let mut x = good.clone();
    x.slice = br#"{"args":{"k":"*"},"fn":"g","fn_hash":"x","ns":"slices.example.org"}"#.to_vec();
    invalid(&x, "the slice bytes are not the slice of the id");
    let mut x = good.clone();
    x.fn_hash = h_of('e');
    invalid(&x, "the function hash is not the slice's");
    let mut x = good.clone();
    x.asof = "2024-01-01".into();
    invalid(&x, "the asof is not an instant");
    let mut x = good.clone();
    x.tx = Some("yesterday".into());
    invalid(&x, "the tx is not an instant");
    assert!(matches!(s.member_set("slice:nope"), Err(datumesh_store::Error::Invalid(_))), "a malformed entry");
    assert_eq!(db.count("member_sets"), 0, "nothing was written");
}

fn the_database_checks_the_entry(b: Backend) {
    // The primary key is checked as 'slice:' || slice_id || ':' || members_hash in the database
    // itself, so a writer bypassing the store cannot plant a row under another entry. The other
    // columns are a real row's (the function stored, the slice's canonical bytes).
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m = manifest(&h, &[h_of('a'), h_of('b')]);
    let (id, mh) = (m.slice().id().to_string(), m.members_hash().to_string());
    let hex: String = m.slice().canonical().iter().map(|x| format!("{x:02x}")).collect();
    let blob = if db.is_pg() { format!("'\\x{hex}'::bytea") } else { format!("X'{hex}'") };
    let insert = |entry: &str| {
        format!(
            "INSERT INTO member_sets (entry, slice_id, members_hash, fn_hash, slice, members, tx, asof, licences) \
             VALUES ('{entry}', '{id}', '{mh}', '{h}', {blob}, 2, '{T1}', '{T2}', '[\"CC0-1.0\"]')"
        )
    };
    let other = h_of('f');
    assert!(db.exec(&insert(&format!("slice:{id}:{other}"))).is_err(), "another members hash in the entry");
    assert!(db.exec(&insert(&format!("slice:{other}:{mh}"))).is_err(), "another slice id in the entry");
    assert!(db.exec(&insert(&format!("{id}:{mh}"))).is_err(), "no prefix");
    assert!(db.exec(&insert(&format!("manifest:{id}:{mh}"))).is_err(), "another prefix");
    assert_eq!(db.count("member_sets"), 0);
    db.exec(&insert(&m.entry())).unwrap();
    assert_eq!(db.count("member_sets"), 1);
    // Twice: the primary key.
    assert!(db.exec(&insert(&m.entry())).is_err());
    assert_eq!(
        s.member_set(&m.entry()).unwrap(),
        Some({
            let mut want = MemberSet::from_manifest(&m, Some(T1), T2);
            want.licences = vec!["CC0-1.0".to_string()];
            want
        }),
        "read by the store, its licences from the JCS text"
    );
}

fn the_table_is_append_only(b: Backend) {
    let db = TestDb::migrated(b);
    let s = slice_store(&db);
    let h = function(&s);
    let m = manifest(&h, &[h_of('a')]);
    assert!(s.put_member_set(&MemberSet::from_manifest(&m, Some(T1), T2)).unwrap());
    db.refused(&format!("UPDATE member_sets SET asof = '{T3}'"));
    db.refused("UPDATE member_sets SET members = 7");
    db.refused("DELETE FROM member_sets");
    if db.is_pg() {
        db.refused("TRUNCATE member_sets");
    } else {
        // SQLite: a replacing insert would delete the row without a DELETE trigger.
        let e = db.exec(&format!(
            "INSERT OR REPLACE INTO member_sets (entry, slice_id, members_hash, fn_hash, slice, members, tx, asof, licences) \
             SELECT entry, slice_id, members_hash, fn_hash, slice, members, tx, '{T3}', licences FROM member_sets"
        ));
        assert!(e.is_err(), "INSERT OR REPLACE refused");
    }
    assert_eq!(db.count("member_sets"), 1);
    assert_eq!(s.member_set(&m.entry()).unwrap().unwrap().asof, T2, "unchanged");
}

// ---- G2 / G7: the migration ------------------------------------------------------------------

fn migrate_to(db: &TestDb, b: Backend, up_to: usize) {
    let old: Vec<&migrate::Migration> = match b {
        Backend::Pg => migrate::MIGRATIONS[..up_to].iter().collect(),
        Backend::Sqlite => migrate::sqlite::MIGRATIONS[..up_to].iter().collect(),
    };
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
}

fn apply_rest(db: &TestDb, b: Backend) -> Vec<u32> {
    match b {
        Backend::Pg => migrate::apply(&db.pg().bare_store()).unwrap(),
        Backend::Sqlite => migrate::sqlite::apply(&db.lite().store_with(SqliteOptions::new())).unwrap(),
    }
}

fn migration_16_and_12_are_the_member_sets(b: Backend) {
    // The member sets are migration 16 (SQLite 12); the recorded time follows (17 / 13, G9).
    let ms = match b {
        Backend::Pg => migrate::MIGRATIONS,
        Backend::Sqlite => migrate::sqlite::MIGRATIONS,
    };
    let want = if b == Backend::Pg { (16, "0016_member_sets.sql") } else { (12, "0012_member_sets.sql") };
    let m = ms.get(want.0 as usize - 1).expect("the migration");
    assert_eq!((m.version, m.name), want);
}

fn a_0_6_4_store_with_a_manifest_migrates_untouched(b: Backend) {
    let (old, new) = if b == Backend::Pg { (15, 16) } else { (11, 12) };
    let db = TestDb::empty(b);
    migrate_to(&db, b, old);
    assert!(columns(&db, "member_sets").is_empty(), "no member_sets before the migration");
    // As store 0.6.4 writes it: members, their manifest, a receipt reading the slice.
    let s = slice_store(&db);
    let h = function(&s);
    let mut hashes = Vec::new();
    for k in 1..=3 {
        hashes.push(put_member(&s, &kcell(&h, k), &(k * 10).to_string(), T1, T1));
    }
    let m = manifest(&h, &hashes);
    assert_eq!(s.put_manifest(&m.canonical()).unwrap(), m.entry());
    let reader = lineage_with(&[&m.entry()], "60", T2, T2);
    let rh = s.put_receipt(&reader).unwrap();
    let before_manifests = dump(
        &db,
        "SELECT entry, slice_id, members_hash, fn_hash, members::text, encode(bytes, 'hex') FROM manifests ORDER BY entry",
        "SELECT entry, slice_id, members_hash, fn_hash, CAST(members AS TEXT), hex(bytes) FROM manifests ORDER BY entry",
    );
    let before_members = dump(
        &db,
        "SELECT entry, member_hash FROM manifest_members ORDER BY entry, member_hash",
        "SELECT entry, member_hash FROM manifest_members ORDER BY entry, member_hash",
    );
    let before_receipts = dump(
        &db,
        "SELECT hash, encode(bytes, 'hex') FROM receipts ORDER BY hash",
        "SELECT hash, hex(bytes) FROM receipts ORDER BY hash",
    );
    assert_eq!((before_manifests.len(), before_members.len()), (1, 3));
    drop(s);

    assert_eq!(
        apply_rest(&db, b)[..2],
        [new, new + 1],
        "the member sets, then the recorded time (G9); later ones follow"
    );

    // G7: nothing stored changes.
    assert_eq!(
        dump(
            &db,
            "SELECT entry, slice_id, members_hash, fn_hash, members::text, encode(bytes, 'hex') FROM manifests ORDER BY entry",
            "SELECT entry, slice_id, members_hash, fn_hash, CAST(members AS TEXT), hex(bytes) FROM manifests ORDER BY entry",
        ),
        before_manifests
    );
    assert_eq!(
        dump(
            &db,
            "SELECT entry, member_hash FROM manifest_members ORDER BY entry, member_hash",
            "SELECT entry, member_hash FROM manifest_members ORDER BY entry, member_hash",
        ),
        before_members
    );
    assert_eq!(
        dump(
            &db,
            "SELECT hash, encode(bytes, 'hex') FROM receipts ORDER BY hash",
            "SELECT hash, hex(bytes) FROM receipts ORDER BY hash",
        ),
        before_receipts
    );
    assert_eq!(db.count("member_sets"), 0, "the migration writes no member-set row (nothing is backfilled)");
    let s = db.store();
    assert_eq!(s.manifest(&m.entry()).unwrap().as_ref(), m.canonical().as_slice(), "the manifest is still served");
    assert_eq!(s.member_manifests(&hashes[0]).unwrap(), [m.entry()]);
    assert_eq!(s.slice_dependents(&m.entry()).unwrap(), std::slice::from_ref(&rh));
    assert_eq!(s.receipt(&rh).unwrap().as_ref(), reader.as_slice(), "the reader's bytes are unchanged");
    assert!(Receipt::from_json(&s.receipt(&rh).unwrap()).unwrap().inputs().contains(&m.entry()));
    assert_eq!(s.member_set(&m.entry()).unwrap(), None);
    // The manifest tables keep their own rules (append-only) after the migration.
    db.refused("DELETE FROM manifests");
    // And the new table works on the migrated store.
    assert!(s.put_member_set(&MemberSet::from_manifest(&m, Some(T1), T2)).unwrap());
    assert_eq!(db.count("member_sets"), 1);
    assert_eq!(db.count("manifests"), 1);
}

backends!(
    the_table_has_the_columns_the_contract_names,
    put_member_set_writes_one_row_and_member_set_reads_it,
    the_row_carries_its_members_licences_distinct_and_sorted,
    put_member_set_is_insert_or_nothing,
    an_empty_slice_has_no_tx,
    a_member_set_and_a_manifest_of_one_entry_coexist,
    malformed_rows_are_refused_by_the_store,
    the_database_checks_the_entry,
    the_table_is_append_only,
    migration_16_and_12_are_the_member_sets,
    a_0_6_4_store_with_a_manifest_migrates_untouched,
);
