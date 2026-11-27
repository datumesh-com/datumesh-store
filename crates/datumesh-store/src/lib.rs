//! A store for [Datumesh](https://datumesh.com): function descriptors, receipts, blobs,
//! versions of cells over transaction time, lineage, namespaces and function names, on
//! PostgreSQL or (since 0.4.0) in one SQLite file. [`PgStore`] and [`SqliteStore`] implement
//! [`Store`], and through it `datumesh_core`'s [`Resolver`], so `datumesh_core::verify` can
//! recompute receipts of stored cells from either; the two behave the same (one
//! conformance suite runs against both).
//!
//! The API is synchronous (the `postgres` driver and a small connection pool; `rusqlite`
//! with SQLite compiled in, feature `sqlite`, on by default). Async callers run it on a
//! blocking thread, e.g. `tokio::task::spawn_blocking`.
//!
//! [`SqliteStore::open`] takes a file path ([`SqliteStore::connect`] a `sqlite:` URL); the
//! file is in WAL mode, every write is one `BEGIN IMMEDIATE` transaction, and the
//! append-only rules are triggers in the file, so they hold for writers in several
//! threads and processes on one machine. Its schema comes from [`migrate::sqlite::apply`].
//!
//! [`PgStore::connect`] / [`PgStore::connect_with`] take a URL and [`PoolOptions`]; TLS
//! (rustls) is used as the URL's `sslmode` says and always verifies the server's
//! certificate and host name, against the Mozilla roots plus those added with
//! [`PoolOptions::tls_root_certificates_pem`]. No type of the driver or of a TLS crate is
//! part of the API, except in [`pg_unstable`] (any connection function, the driver
//! itself), which is outside semver until 1.0.
//!
//! What is stored, and how:
//!
//! * Descriptors are checked by the core (`FunctionDescriptor::from_json`: since 0.3.1,
//!   spec-0.4.0's, with `signature.order` and argument `extent`) and stored in canonical
//!   form (JCS) under their hash. Receipts are verified (`datumesh_core::verify`, SPEC 10.1
//!   steps 1-6) against the key document stored for their signer host, so nothing
//!   unverified enters the store, and are stored canonical under their hash. Storing the
//!   same thing twice stores it once. A second, different valid signature over the same
//!   payload (possible only with a randomised signer: the payload names the key, so a
//!   rotated key gives a new hash) is refused ([`Error::Conflict`]) and the stored receipt
//!   kept.
//! * Blobs are stored in the database, under the SHA-256 of their content.
//! * Versions: from transaction time `tx` on, cell `cell_id` has the value of a receipt.
//!   Append-only and in tx order per cell, enforced by the database (triggers) as well as by
//!   [`Store::put_version`], which also checks that the receipt is stored and is of that
//!   cell at that tx.
//! * Computed versions: from transaction time `tx` on, cell `cell_id` has the value a
//!   receipt computed, as of the receipt's asof. Derived rather than facts, so they are
//!   added in any order, keyed on (cell, tx, asof), insert-only
//!   ([`Store::put_computed`], [`Store::computed_asof`]).
//! * Answering as of A: a receipt answers as of A only when its `tx` and its `asof` are
//!   both at or before A ([`Store::stored_asof`], and the [`Resolver`]'s `evaluate` of a
//!   stored function; [`Store::version_asof`] looks at `tx` only).
//! * Lineage: `put_receipt` records an edge to each of the receipt's `inputs`, held or not:
//!   receipt hashes ([`Store::inputs`], [`Store::dependents`]) and, since 0.3.1, slice
//!   entries ([`Store::slice_entries`], [`Store::slice_dependents`]);
//!   [`Store::cell_dependents`] lifts both to cells.
//! * Slices (0.3.1, SPEC 15.13): every stored receipt's cell is indexed by its typed
//!   arguments, so that [`Store::slice_members`] finds the members of a slice as of A with
//!   a range scan, in key order; [`Store::versions_between`] gives a version slice.
//!   [`Store::slice_values`] gives the members' values without receipts (0.3.2) and
//!   [`Store::slice_changed_after`] whether they may answer differently later.
//!   Manifests ([`Store::put_manifest`], served by [`Resolver::manifest`]) are kept by
//!   entry and found from each member ([`Store::member_manifests`]). A receipt citing a
//!   slice can be stored before its manifest, and a manifest before its members.
//! * Namespaces: a host's key document (checked as SPEC 9.2 requires), with when it was
//!   validated. Store it before the host's receipts. Replacing it later (a key revoked or
//!   dropped) does not touch receipts already stored: they verified when they were stored,
//!   and putting one of them again is still idempotent.
//! * Names: which function hash a fully qualified function name meant from a transaction
//!   time on. Append-only, like versions. [`Store::resolve_binding`] gives the binding
//!   answering as of A with when it began; [`Store::resolve_name`] its hash.
//! * Classes (0.4.1, SPEC 16.1 to 16.3): a flattened class descriptor, checked by the core and
//!   stored canonical under its class hash ([`Store::put_class`], [`Store::class`]). A class
//!   name is bound over transaction time to a class hash, and binding it binds each method
//!   `<class name>/<m>` to the method's function hash in the same transaction
//!   ([`Store::bind_class`], [`Store::resolve_class`], [`Store::resolve_class_binding`]).
//!   Class names and function names are one namespace: a name bound as one kind cannot be
//!   bound as the other ([`Error::NameTaken`]).
//! * Owner: a class's owner is the definer's check (SPEC 16.2 step 1), made when the class is
//!   defined, not the store's. The store records what it is given; the HTTP service and the
//!   engine enforce the owner rule. [`Store::put_class`] and [`Store::bind_class`] do not take a
//!   key document or check the name against the steward.
//! * Namespace suspension (since 0.4.1): new writes under a suspended namespace are refused
//!   with [`Error::NamespaceSuspended`] (`put_class`: the steward's namespace; `bind_class`: the
//!   host of the class name); reads are never refused.
//!
//! Instants are compared as text (fixed width, `COLLATE "C"`; SQLite's default collation
//! is the same byte order), which is exact; on PostgreSQL each is also stored as
//! `timestamptz`, which round-trips every instant of the specification exactly. Numbers in
//! the typed-argument index compare as exact decimals (PostgreSQL `numeric`; on SQLite a
//! text encoding that sorts as the decimal does).
//!
//! Every [`Store`] method returns a `Result`: a malformed argument is [`Error::Invalid`] with
//! the core's code, a database failure [`Error::Database`]. The [`Resolver`] methods keep the
//! core's `Option` signatures; there a failure is logged with `tracing` (target
//! `datumesh_store`) and answered `None`, which `verify` reports as unavailable.
//!
//! [`Store`] is sealed: only this crate implements it. The store is the trust boundary:
//! everything in it was checked on the way in (every receipt verified against its host's
//! key document), and its callers rely on that rather than checking again. A `dyn Store`
//! is still a trait object as before.
//!
//! The schema comes from [`migrate::apply`] (PostgreSQL) or [`migrate::sqlite::apply`]
//! (SQLite) (the `datumesh-store migrate` command, for either), through a store on the
//! (possibly empty) database.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::borrow::Cow;

use datumesh_core::lexical::{check_hex, check_instant};
pub use datumesh_core::Resolver;
use datumesh_core::{Cell, Scalar, Slice};

mod catalogue;
mod error;
mod functions;
mod listings;
pub mod migrate;
mod pg;
pub mod pg_unstable;
mod pool;
mod slices;
mod sql;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "sqlite")]
mod sqlite_sql;
mod tls;

/// The clock a store records its versions at (CONTRACT-guard-patch G9): an instant, read at each
/// insert. The store sets `recorded = max(signed tx, clock())`; with no clock it is the signed tx.
/// Set once, when the store is opened ([`PoolOptions::recording_clock`], [`SqliteOptions::recording_clock`]).
/// Since 0.6.5.
pub type RecordingClock = std::sync::Arc<dyn Fn() -> String + Send + Sync>;

/// The instant one microsecond after `s` (a `YYYY-MM-DDTHH:MM:SS.ffffffZ` instant, carried through the
/// calendar; `s` unchanged when it is not one). The store's per-cell rule of recorded times (decided 27
/// Nov, B5) uses it: a version is recorded after the cell's latest.
pub(crate) fn instant_after(s: &str) -> String {
    if s.len() != 27 || !s.is_ascii() || s.as_bytes()[19] != b'.' || s.as_bytes()[26] != b'Z' {
        return s.to_string();
    }
    let num = |r: std::ops::Range<usize>| s[r].parse::<u64>().ok();
    let (Some(mut y), Some(mut mo), Some(mut d), Some(mut h), Some(mut mi), Some(mut se), Some(mut us)) =
        (num(0..4), num(5..7), num(8..10), num(11..13), num(14..16), num(17..19), num(20..26))
    else {
        return s.to_string();
    };
    us += 1;
    if us == 1_000_000 {
        us = 0;
        se += 1;
    }
    if se == 60 {
        se = 0;
        mi += 1;
    }
    if mi == 60 {
        mi = 0;
        h += 1;
    }
    if h == 24 {
        h = 0;
        d += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return s.to_string(),
    };
    if d > dim {
        d = 1;
        mo += 1;
    }
    if mo == 13 {
        mo = 1;
        y += 1;
    }
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}.{us:06}Z")
}

/// The later of two instants of the same fixed-width form (their text order).
pub(crate) fn later_of(a: &str, b: &str) -> String {
    if a >= b {
        a.to_string()
    } else {
        b.to_string()
    }
}

/// The recorded time of a version signed at `tx`: the greater of `tx` and the store's clock, or
/// `tx` with no clock (SPEC-free: a store detail, see [`Version::recorded`]).
pub(crate) fn recorded_at(clock: Option<&RecordingClock>, tx: &str) -> String {
    match clock {
        Some(now) => {
            let now = now();
            if now.as_str() > tx {
                now
            } else {
                tx.to_string()
            }
        }
        None => tx.to_string(),
    }
}

pub use catalogue::{
    CatalogEntry, CatalogEntryExt, CatalogKind, CatalogSource, Dictionary, DictionaryValue, EmbedRef, Embedding,
    FacetedEntry, Facets, Hit, KeyValue, PendingEmbedding, Summary, SummaryEntry, FIND_MISSES_CAP,
};
pub use error::{DatabaseError, Error, Result};
pub use listings::{ListedHost, Listing, ListingWrite, SubjectKeys, LISTING_SUBJECTS_MAX};
pub use pg::PgStore;
pub use pool::PoolOptions;
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteOptions, SqliteStore};

/// The sort key of a cell (CONTRACT-0.5.1 S1; SPEC 5.6 and 15.13): its arguments as
/// `(name, type, lexical form)`, in ascending name order, encoded as `cells.sort_key` is stored
/// (the same bytes on PostgreSQL and SQLite) and as [`SliceValue::sort_key`] returns them. Keys
/// compare as bytes. An empty `args` gives the empty key of a cell with no arguments. Since
/// 0.6.4.
pub fn sort_key(args: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut sorted: Vec<&(&str, &str, &str)> = args.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    sorted.iter().flat_map(|(_, ty, value)| slices::sort_item(ty, value)).collect()
}

/// A member-set row (CONTRACT-guard-patch G2, since 0.6.5): what a slice read saw, without its
/// members. The entry is `slice:<slice_id>:<members_hash>`, as a manifest's; `slice` is the slice
/// object's canonical bytes, `members` the count, `tx` the latest member or withdrawal tx the read
/// saw (`None`: none, an empty slice) and `asof` the read's as-of. Written once per entry
/// ([`Store::put_member_set`]).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberSet {
    /// `slice:<slice_id>:<members_hash>`.
    pub entry: String,
    /// The slice's id.
    pub slice_id: String,
    /// The members hash (SPEC 15.13): the entry's second part.
    pub members_hash: String,
    /// The slice's function hash (of the version slice's cell for a version slice).
    pub fn_hash: String,
    /// The slice object's canonical bytes (the members are rebuilt from them).
    pub slice: Vec<u8>,
    /// How many members.
    pub members: u64,
    /// The latest member or withdrawal tx the read saw; `None` when there is none.
    pub tx: Option<String>,
    /// The read's as-of.
    pub asof: String,
    /// The members' distinct licence URLs, sorted (decided 26 Nov, G3): the store keeps them
    /// distinct and sorted, and refuses an empty one. `from_manifest` leaves them empty; the caller
    /// sets them (the reuse check reads them from the row, no member).
    pub licences: Vec<String>,
    /// The slice's [`Store::slice_seq`] read before its members (decided 26 Nov, G3): the reuse check
    /// stands only while the slice's seq is still this. `from_manifest` leaves it 0; the caller sets it.
    pub seen_seq: i64,
}

impl MemberSet {
    /// The row of `manifest`, read at `asof`, whose latest member or withdrawal tx is `tx`.
    pub fn from_manifest(manifest: &datumesh_core::Manifest, tx: Option<&str>, asof: &str) -> MemberSet {
        let slice = manifest.slice();
        MemberSet {
            entry: manifest.entry(),
            slice_id: slice.id().to_string(),
            members_hash: manifest.members_hash().to_string(),
            fn_hash: slice.fn_hash().to_string(),
            slice: slice.canonical(),
            members: manifest.members().len() as u64,
            tx: tx.map(str::to_string),
            asof: asof.to_string(),
            licences: Vec::new(),
            seen_seq: 0,
        }
    }
}

/// Checks a member set's entry (`slice:<64 hex>:<64 hex>`): its two ids, and the entry's form.
pub(crate) fn check_member_entry(entry: &str) -> Result<(&str, &str)> {
    let bad = || Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "a member-set entry"));
    let rest = entry.strip_prefix("slice:").ok_or_else(bad)?;
    let (id, members_hash) = rest.split_once(':').ok_or_else(bad)?;
    check_hex(id, 64, "slice_id")?;
    check_hex(members_hash, 64, "members_hash")?;
    Ok((id, members_hash))
}

/// The receipt hashes of `members` that are not withdrawn, at most `limit`, in no particular order: the
/// result of `slice_member_hashes` for a version slice (and of the default). Withdrawals are left out, as
/// `slice_values` leaves them out (decided 27 Nov, A2).
pub(crate) fn hashes_limited(members: Vec<SliceMember>, limit: usize) -> Vec<String> {
    let mut members: Vec<SliceMember> = members.into_iter().filter(|m| !m.version.withdrawn).collect();
    members.truncate(limit);
    members.into_iter().map(|m| m.version.receipt_hash).collect()
}

/// The licences of a member set as the store keeps them: distinct and sorted; an empty one refused.
pub(crate) fn normal_licences(list: &[String]) -> Result<Vec<String>> {
    if list.iter().any(|l| l.is_empty()) {
        return Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "an empty licence",
        )));
    }
    let mut out = list.to_vec();
    out.sort();
    out.dedup();
    Ok(out)
}

/// The JCS text of a (normalised) licence list, as `member_sets.licences` holds it.
pub(crate) fn licences_text(list: &[String]) -> String {
    let arr =
        datumesh_core::json::Value::Array(list.iter().map(|s| datumesh_core::json::Value::String(s.clone())).collect());
    String::from_utf8(arr.to_jcs()).unwrap_or_default()
}

/// The licence list of `member_sets.licences`' JCS text.
pub(crate) fn licences_of(text: &str) -> Result<Vec<String>> {
    let bad =
        || Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "a stored licence list"));
    match datumesh_core::json::parse(text.as_bytes()).map_err(Error::Invalid)? {
        datumesh_core::json::Value::Array(items) => {
            items.iter().map(|i| i.as_str().map(str::to_string).ok_or_else(bad)).collect()
        }
        _ => Err(bad()),
    }
}

/// Checks a member-set row as the store accepts it (G2): its entry, the ids and function hash,
/// the slice bytes (a canonical slice object of the slice id and function hash) and the instants.
pub(crate) fn check_member_set(set: &MemberSet) -> Result<()> {
    let (id, members_hash) = check_member_entry(&set.entry)?;
    if set.members_hash != members_hash {
        return Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "members_hash is not the entry's",
        )));
    }
    if set.slice_id != id {
        return Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "slice_id is not the entry's",
        )));
    }
    check_hex(&set.fn_hash, 64, "fn_hash")?;
    let slice = Slice::from_json(&set.slice).map_err(Error::Invalid)?;
    if slice.id() != set.slice_id || slice.fn_hash() != set.fn_hash || slice.canonical() != set.slice {
        return Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "the slice bytes are not the canonical slice of the id and function hash",
        )));
    }
    check_instant(&set.asof, "asof")?;
    if let Some(tx) = &set.tx {
        check_instant(tx, "tx")?;
    }
    if set.members > 1_048_576 {
        return Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "members is over the limit",
        )));
    }
    Ok(())
}

/// A version of a cell: from transaction time `tx` on, the cell's value is that of the
/// receipt `receipt_hash`, which states it as of `asof`.
///
/// A stored version ([`Store::put_version`]) or a computed one ([`Store::put_computed`]).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    /// The cell id.
    pub cell_id: String,
    /// The transaction time (an instant, SPEC 3.3).
    pub tx: String,
    /// The receipt stating the value.
    pub receipt_hash: String,
    /// The receipt's `asof` (an instant; never earlier than `tx`). Since 0.2.0.
    pub asof: String,
    /// Whether the receipt is a withdrawal (8.1): the cell has no value from `asof` on. Since
    /// 0.6.0; always `false` for a computed version.
    pub withdrawn: bool,
    /// The instant the store recorded the version at (CONTRACT-guard-patch G9): the greater of
    /// `tx` and the store's recording clock, or `tx` when the store has none. Every as-of
    /// answer is by this time; a computed version's is its `tx`. Since 0.6.5.
    pub recorded: String,
    /// The insertion sequence of the version: assigned by the store at insert (through `put_version`
    /// and `put_cells`), only increasing, never updated; withdrawals included. A computed version's
    /// is 0. Since 0.6.5 (G3, "inserted since"; see [`Store::slice_seq`]).
    pub seq: i64,
}

/// One cell write of [`Store::put_cells`]: a receipt and, optionally, the version it
/// becomes. Since 0.4.3.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellWrite<'a> {
    /// The receipt's JSON bytes, as for `put_receipt`.
    pub receipt: &'a [u8],
    /// `Some((cell_id, tx))`: also `put_version(cell_id, tx, <this receipt's hash>)`.
    pub version: Option<(&'a str, &'a str)>,
}

impl<'a> CellWrite<'a> {
    /// A cell write of `receipt` and, when `version` is `Some((cell_id, tx))`, its version.
    pub fn new(receipt: &'a [u8], version: Option<(&'a str, &'a str)>) -> Self {
        CellWrite { receipt, version }
    }
}

/// A member of a slice ([`Store::slice_members`], [`Store::versions_between`]): a stored
/// version and the bytes of its receipt (as [`receipt`](Resolver::receipt) returns them).
/// Since 0.3.1.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SliceMember {
    /// The version answering (its cell, tx, receipt hash and asof).
    pub version: Version,
    /// The receipt's bytes, canonical JSON.
    pub receipt: Vec<u8>,
}

/// A member of a slice with its value and without its receipt's bytes
/// ([`Store::slice_values`]). Since 0.3.2.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SliceValue {
    /// The version answering (its cell, tx, receipt hash and asof).
    pub version: Version,
    /// The receipt's typed value.
    pub value: Scalar,
    /// The member's cell, with its arguments (from the store's typed-argument index; equal
    /// to the receipt's `cell`, and `cell.id()` is `version.cell_id`).
    pub cell: Cell,
    /// The member's sort key (the cell's, see [`Store::slice_page`]): the byte order of the
    /// keys is the order of [`Store::slice_values`]. Since 0.6.4.
    pub sort_key: Vec<u8>,
}

/// A name binding: from transaction time `tx` on, a fully qualified function name means the
/// function `fn_hash` ([`Store::bind_name`]). Since 0.3.0.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    /// The function hash the name means.
    pub fn_hash: String,
    /// When the binding began: the transaction time from which the name means `fn_hash`
    /// (an instant, SPEC 3.3).
    pub tx: String,
}

/// A class-name binding: from transaction time `tx` on, a class name means the class
/// `class_hash` ([`Store::bind_class`]). The class counterpart of [`Binding`]; the bindings
/// answer of SPEC 9.7 gives `class_hash` in place of `fn_hash` for such a name. Since 0.4.1.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClassBinding {
    /// The class hash the name means.
    pub class_hash: String,
    /// When the binding began (an instant, SPEC 3.3).
    pub tx: String,
}

/// A function test: a canonical test line of a function, stored once per (function hash,
/// case) ([`Store::put_function_test`]). The run fields are set by
/// [`Store::record_function_test`]; the branch counts by [`Store::set_function_branches`],
/// which keeps them equal on every test of the function. Since 0.5.1.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionTest {
    /// The function hash the test is of (64 lowercase hex digits).
    pub fn_hash: String,
    /// The case: 16 lowercase hex digits. Tests of a function are ordered by it.
    pub case: String,
    /// The test line, canonical (JCS) JSON.
    pub test_json: Vec<u8>,
    /// The host or key id that wrote the test. The first writer's: a later one changes nothing.
    pub signed_by: String,
    /// When the test last ran (an instant), if it has.
    pub last_run_at: Option<String>,
    /// The result of the last run: `pass`, `fail` or `error`.
    pub last_result: Option<String>,
    /// The detail of the last run.
    pub last_detail: Option<String>,
    /// The branches of the function (the same on every test of the function).
    pub branches_total: u32,
    /// The branches the function's tests cover (the same on every test of the function).
    pub branches_covered: u32,
}

impl FunctionTest {
    /// A test that has not run, with no branch counts: the run fields are `None` and the
    /// counts zero. Since 0.5.1.
    pub fn new(fn_hash: &str, case: &str, test_json: &[u8], signed_by: &str) -> FunctionTest {
        FunctionTest {
            fn_hash: fn_hash.to_string(),
            case: case.to_string(),
            test_json: test_json.to_vec(),
            signed_by: signed_by.to_string(),
            last_run_at: None,
            last_result: None,
            last_detail: None,
            branches_total: 0,
            branches_covered: 0,
        }
    }
}

/// The results of a function's tests ([`Store::function_test_summary`]): how many there are,
/// how many passed, failed, errored or have not run, the latest run and the branch counts.
/// `Default` is the summary of no tests (all zero, no run). Since 0.5.2.
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct TestSummary {
    /// The tests counted.
    pub count: u64,
    /// The tests whose last run passed.
    pub pass: u64,
    /// The tests whose last run failed.
    pub fail: u64,
    /// The tests whose last run errored.
    pub error: u64,
    /// The tests that have not run (no last result).
    pub not_run: u64,
    /// The latest `last_run_at` of the tests, if any has run (an instant; instants compare as
    /// text).
    pub last_run_at: Option<String>,
    /// The branches of the function (the value replicated on its tests; 0 when none).
    pub branches_total: u32,
    /// The branches covered by the function's tests (replicated as `branches_total`).
    pub branches_covered: u32,
}

/// A namespace's host and its key document.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Namespace {
    /// The host (a DNS name).
    pub host: String,
    /// The key document, canonical (JCS).
    pub keydoc: Vec<u8>,
    /// When the document was last retrieved and checked.
    pub validated_at: String,
    /// When the namespace was suspended, if it is: the single record of suspension. Set by
    /// [`record_namespace_failure`](Store::record_namespace_failure) when the consecutive
    /// failures reach its threshold, cleared by
    /// [`record_namespace_success`](Store::record_namespace_success). While it is set the
    /// store refuses new writes under the host ([`Error::NamespaceSuspended`]).
    pub suspended_at: Option<String>,
    /// How many checks of the host's key document have failed in a row (zero after a
    /// success). Since the namespace checks (since 0.4.1).
    pub consecutive_failures: u32,
    /// When the host's key document was last checked, successfully or not; `None` before
    /// the first check recorded. Since the namespace checks (since 0.4.1).
    pub last_checked_at: Option<String>,
}

/// The namespace after a check at `at`: a failed one (`threshold` is `Some`) adds to the
/// consecutive failures and suspends at the threshold (an existing suspension keeps its
/// time), a successful one (`None`) resets them and reinstates.
/// The refusal of class descriptor bytes that are not the canonical form of the descriptor
/// (`E_SCHEMA`, nothing stored): the class hash is the hash of the bytes stored, so only the
/// canonical bytes are stored. Shared by both backends (since 0.4.1).
pub(crate) fn not_canonical_class() -> Error {
    Error::Invalid(datumesh_core::Error::new(
        datumesh_core::ErrorCode::Schema,
        "$",
        "class descriptor is not in canonical form (JCS): put the canonical bytes",
    ))
}

pub(crate) fn apply_check(ns: &mut Namespace, at: &str, threshold: Option<u32>) {
    ns.last_checked_at = Some(at.to_string());
    match threshold {
        None => {
            ns.consecutive_failures = 0;
            ns.suspended_at = None;
        }
        Some(t) => {
            ns.consecutive_failures = ns.consecutive_failures.saturating_add(1);
            if ns.suspended_at.is_none() && ns.consecutive_failures >= t.max(1) {
                ns.suspended_at = Some(at.to_string());
            }
        }
    }
}

/// A Datumesh store: a [`Resolver`] that can also be written to.
///
/// Instants are SPEC 3.3 instants (`YYYY-MM-DDTHH:MM:SS.ffffffZ`), hashes and cell ids 64
/// lowercase hex digits, hosts DNS names and function names fully qualified. Every method,
/// read or write, refuses anything else with the core's error code ([`Error::Invalid`]),
/// and reports a database failure as [`Error::Database`] (or [`Error::PoolTimeout`]): a read
/// answers `Ok(None)` / `Ok(vec![])` only when the store really holds nothing.
///
/// The [`Resolver`] methods keep the core's `Option` signatures: there a malformed argument
/// or a database failure is `None` (logged with `tracing` when it is a failure), which
/// `verify` reports as unavailable.
///
/// Only this crate implements `Store` (it is sealed, since 0.3.0; [`PgStore`] and, since
/// 0.4.0, [`SqliteStore`]): the store is the trust boundary, where every receipt is verified against its host's key document before it is
/// kept, and what a store answers is relied on as checked. An implementation elsewhere
/// could answer anything. `Store` stays object-safe (`Arc<dyn Store + Send + Sync>`). Even a complete implementation that
/// delegates every method to a [`PgStore`] is refused (`E0277`: the private supertrait is
/// not implemented; `tests/sealed.rs` checks that this is the only error):
///
/// ```compile_fail
/// use std::borrow::Cow;
/// use datumesh_core::{Cell, Scalar};
/// use datumesh_core::Slice;
/// use datumesh_store::{Binding, CatalogEntry, ClassBinding, Embedding, FunctionTest, Hit, KeyValue, Namespace, PendingEmbedding, PgStore, Resolver, Result, SliceMember, SliceValue, Store, Summary, Version};
///
/// struct Wrapper(PgStore);
/// impl Resolver for Wrapper {
///     fn descriptor(&self, h: &str) -> Option<Cow<'_, [u8]>> { self.0.descriptor(h) }
///     fn receipt(&self, h: &str) -> Option<Cow<'_, [u8]>> { self.0.receipt(h) }
///     fn evaluate(&self, c: &Cell, a: &str) -> Option<Scalar> { self.0.evaluate(c, a) }
///     fn manifest(&self, e: &str) -> Option<Cow<'_, [u8]>> { self.0.manifest(e) }
/// }
/// impl Store for Wrapper {
///     fn put_descriptor(&self, b: &[u8]) -> Result<String> { self.0.put_descriptor(b) }
///     fn put_receipt(&self, b: &[u8]) -> Result<String> { self.0.put_receipt(b) }
///     fn put_blob(&self, b: &[u8]) -> Result<String> { self.0.put_blob(b) }
///     fn blob(&self, s: &str) -> Result<Option<Cow<'_, [u8]>>> { self.0.blob(s) }
///     fn put_version(&self, c: &str, t: &str, r: &str) -> Result<()> { self.0.put_version(c, t, r) }
///     fn version_asof(&self, c: &str, a: &str) -> Result<Option<Version>> { self.0.version_asof(c, a) }
///     fn versions(&self, c: &str, f: &str, t: &str) -> Result<Vec<Version>> { self.0.versions(c, f, t) }
///     fn latest(&self, c: &str) -> Result<Option<Version>> { self.0.latest(c) }
///     fn dependents(&self, r: &str) -> Result<Vec<String>> { self.0.dependents(r) }
///     fn inputs(&self, r: &str) -> Result<Vec<String>> { self.0.inputs(r) }
///     fn put_namespace(&self, h: &str, k: &[u8], v: &str) -> Result<()> { self.0.put_namespace(h, k, v) }
///     fn namespace(&self, h: &str) -> Result<Option<Namespace>> { self.0.namespace(h) }
///     fn namespaces(&self) -> Result<Vec<Namespace>> { self.0.namespaces() }
///     fn record_namespace_failure(&self, h: &str, a: &str, n: u32) -> Result<Option<Namespace>> { self.0.record_namespace_failure(h, a, n) }
///     fn record_namespace_success(&self, h: &str, a: &str) -> Result<Option<Namespace>> { self.0.record_namespace_success(h, a) }
///     fn bind_name(&self, n: &str, f: &str, t: &str) -> Result<()> { self.0.bind_name(n, f, t) }
///     fn resolve_name(&self, n: &str, a: &str) -> Result<Option<String>> { self.0.resolve_name(n, a) }
///     fn resolve_binding(&self, n: &str, a: &str) -> Result<Option<Binding>> { self.0.resolve_binding(n, a) }
///     fn put_class(&self, b: &[u8]) -> Result<String> { self.0.put_class(b) }
///     fn class(&self, h: &str) -> Result<Option<Vec<u8>>> { self.0.class(h) }
///     fn bind_class(&self, n: &str, h: &str, t: &str) -> Result<()> { self.0.bind_class(n, h, t) }
///     fn resolve_class(&self, n: &str, a: &str) -> Result<Option<String>> { self.0.resolve_class(n, a) }
///     fn resolve_class_binding(&self, n: &str, a: &str) -> Result<Option<ClassBinding>> { self.0.resolve_class_binding(n, a) }
///     fn put_computed(&self, c: &str, t: &str, r: &str) -> Result<()> { self.0.put_computed(c, t, r) }
///     fn computed_asof(&self, c: &str, a: &str) -> Result<Option<Version>> { self.0.computed_asof(c, a) }
///     fn stored_asof(&self, c: &str, a: &str) -> Result<Option<Version>> { self.0.stored_asof(c, a) }
///     fn cell_dependents(&self, c: &str) -> Result<Vec<String>> { self.0.cell_dependents(c) }
///     fn put_manifest(&self, b: &[u8]) -> Result<String> { self.0.put_manifest(b) }
///     fn member_manifests(&self, r: &str) -> Result<Vec<String>> { self.0.member_manifests(r) }
///     fn slice_entries(&self, r: &str) -> Result<Vec<String>> { self.0.slice_entries(r) }
///     fn slice_dependents(&self, e: &str) -> Result<Vec<String>> { self.0.slice_dependents(e) }
///     fn slice_members(&self, s: &Slice, a: &str) -> Result<Vec<SliceMember>> { self.0.slice_members(s, a) }
///     fn versions_between(&self, c: &str, f: &str, t: &str, a: &str) -> Result<Vec<SliceMember>> { self.0.versions_between(c, f, t, a) }
///     fn slice_values(&self, s: &Slice, a: &str) -> Result<Vec<SliceValue>> { self.0.slice_values(s, a) }
///     fn slice_changed_after(&self, s: &Slice, a: &str) -> Result<bool> { self.0.slice_changed_after(s, a) }
///     fn computed_cells(&self, s: &Slice, a: &str, n: usize) -> Result<Vec<Cell>> { self.0.computed_cells(s, a, n) }
///     fn has_bindings_under(&self, n: &str) -> Result<bool> { self.0.has_bindings_under(n) }
///     fn upsert_catalog(&self, e: &[CatalogEntry]) -> Result<Vec<i64>> { self.0.upsert_catalog(e) }
///     fn upsert_key_values(&self, o: &str, k: &str, v: &[KeyValue]) -> Result<Vec<i64>> { self.0.upsert_key_values(o, k, v) }
///     fn refresh_value_counts(&self) -> Result<u64> { self.0.refresh_value_counts() }
///     fn text_find(&self, q: &str, l: usize) -> Result<Vec<Hit>> { self.0.text_find(q, l) }
///     fn vector_find(&self, m: &str, v: &[f32], l: usize) -> Result<Vec<Hit>> { self.0.vector_find(m, v, l) }
///     fn put_embeddings(&self, m: &str, i: &[Embedding]) -> Result<()> { self.0.put_embeddings(m, i) }
///     fn pending_embeddings(&self, m: &str, l: usize) -> Result<Vec<PendingEmbedding>> { self.0.pending_embeddings(m, l) }
///     fn vector_available(&self) -> bool { self.0.vector_available() }
///     fn log_miss(&self, q: &str) -> Result<()> { self.0.log_miss(q) }
///     fn catalog_summary(&self) -> Result<Summary> { self.0.catalog_summary() }
///     fn put_function_test(&self, t: &FunctionTest) -> Result<bool> { self.0.put_function_test(t) }
///     fn function_tests(&self, f: &str) -> Result<Vec<FunctionTest>> { self.0.function_tests(f) }
///     fn record_function_test(&self, f: &str, c: &str, r: &str, res: &str, d: &str) -> Result<bool> { self.0.record_function_test(f, c, r, res, d) }
///     fn set_function_branches(&self, f: &str, t: u32, c: u32) -> Result<()> { self.0.set_function_branches(f, t, c) }
///     fn function_readers(&self, n: &str) -> Result<Vec<String>> { self.0.function_readers(n) }
///     fn bound_names(&self, f: &str) -> Result<Vec<String>> { self.0.bound_names(f) }
/// }
/// ```
pub trait Store: Resolver + sealed::Sealed {
    /// Writes the member-set row (G2, since 0.6.5): inserted when no row with its entry is held
    /// (`Ok(true)`), otherwise nothing (`Ok(false)`, the held row kept). A row that is not
    /// consistent (its entry, slice bytes, function hash or instants) is [`Error::Invalid`].
    fn put_member_set(&self, set: &MemberSet) -> Result<bool> {
        let mut set = set.clone();
        set.licences = normal_licences(&set.licences)?;
        check_member_set(&set)?;
        self.put_member_set_checked(&set)
    }

    /// The highest insertion seq among the versions (withdrawals included) of the cells a slice
    /// matches, whatever their recorded time; 0 when there are none. For a version slice, its cell's
    /// versions (decided 26 Nov, G3: "inserted since", index-backed). Since 0.6.5.
    fn slice_seq(&self, slice: &Slice) -> Result<i64> {
        self.slice_seq_checked(slice)
    }

    /// The time a version signed at `signed_tx` is recorded at by the next put (G9): the greater of
    /// `signed_tx` and this store's recording clock, or `signed_tx` with no clock. Exactly the rule the
    /// puts apply, so publish-stored can compare against it. Since 0.6.5.
    ///
    /// Decided 27 Nov (B5): the puts also record a version after its cell's latest recorded time, a
    /// microsecond later, so the time a put of a cell that already has versions records is later still.
    /// This method gives the signed-tx-and-clock part only; a per-cell check must use the cell's latest.
    fn recording_time(&self, signed_tx: &str) -> String {
        crate::recorded_at(self.recording_clock_of().as_ref(), signed_tx)
    }

    /// The store's recording clock, not part of the API: the store's own implementations override it.
    #[doc(hidden)]
    fn recording_clock_of(&self) -> Option<RecordingClock> {
        None
    }

    /// Waits for the writers of versions in flight, and returns at once when there is none (review item
    /// 2): once it has passed, any writer still to come records a time later than it. The engine calls it
    /// once per request, before its first store read. Since 0.6.5.
    fn barrier(&self) -> Result<()> {
        self.barrier_checked()
    }

    /// The read of `barrier`, not part of the API: the store's own implementations override it.
    #[doc(hidden)]
    fn barrier_checked(&self) -> Result<()> {
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no barrier here")))
    }

    /// The receipt hashes of the members of a slice as of `asof`, at most `limit`, in no particular order
    /// (the engine sorts them, and uses the limit only to see whether there are more). Members are selected
    /// as of `asof` by recorded time, the rules of `slice_members`; no values or receipt bytes are read.
    /// Since 0.6.5.
    fn slice_member_hashes(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<String>> {
        Ok(hashes_limited(self.slice_members(slice, asof)?, limit))
    }

    /// The least start, over the versions of `cell_id` recorded after `recorded`, of `max(receipt asof,
    /// recorded)`; `None` when none is recorded after (decided 27 Nov, A1: the memo bound, one aggregate
    /// row). Since 0.6.5.
    fn later_bound(&self, cell_id: &str, recorded: &str) -> Result<Option<String>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(recorded, "recorded")?;
        self.later_bound_checked(cell_id, recorded)
    }

    /// The read of `later_bound`, not part of the API: the store's own implementations override it.
    #[doc(hidden)]
    fn later_bound_checked(&self, cell_id: &str, recorded: &str) -> Result<Option<String>> {
        let _ = (cell_id, recorded);
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no versions here")))
    }

    /// The receipt hashes of the members of a slice as of `asof`, at most `limit`, within `timeout`
    /// (decided 27 Nov, B6: the rebuild listing under the budget's remaining time). On PostgreSQL the
    /// statement runs under `statement_timeout`; on SQLite a progress deadline stops it. Past the timeout
    /// the call fails with [`Error::Timeout`]. Since 0.6.5.
    fn slice_member_hashes_within(
        &self,
        slice: &Slice,
        asof: &str,
        limit: usize,
        timeout: std::time::Duration,
    ) -> Result<Vec<String>> {
        self.slice_member_hashes_within_checked(slice, asof, limit, timeout)
    }

    /// The read of `slice_member_hashes_within`, not part of the API. The default (no time bound) is the
    /// listing of `slice_member_hashes`.
    #[doc(hidden)]
    fn slice_member_hashes_within_checked(
        &self,
        slice: &Slice,
        asof: &str,
        limit: usize,
        timeout: std::time::Duration,
    ) -> Result<Vec<String>> {
        let _ = timeout;
        self.slice_member_hashes(slice, asof, limit)
    }

    /// The first version of `cell_id` recorded after `recorded` (one row, the memo bound), or `None`.
    /// Since 0.6.5.
    fn next_version_after(&self, cell_id: &str, recorded: &str) -> Result<Option<Version>> {
        check_hex(cell_id, 64, "cell_id")?;
        check_instant(recorded, "recorded")?;
        self.next_version_after_checked(cell_id, recorded)
    }

    /// The read of `next_version_after`, not part of the API: the store's own implementations override it.
    #[doc(hidden)]
    fn next_version_after_checked(&self, cell_id: &str, recorded: &str) -> Result<Option<Version>> {
        let _ = (cell_id, recorded);
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no versions here")))
    }

    /// The highest insertion seq among the versions (withdrawals included) of the functions `fn_hash`,
    /// 0 when there are none: an upper bound of `slice_seq` for every slice of that function, in one
    /// probe of the function's row (decided 26 Nov, G3: a slice whose `fn_seq` is at or below its row's
    /// `seen_seq` is unchanged, with no candidate walk). Since 0.6.5.
    fn fn_seq(&self, fn_hash: &str) -> Result<i64> {
        check_hex(fn_hash, 64, "fn_hash")?;
        self.fn_seq_checked(fn_hash)
    }

    /// The read of `fn_seq`, not part of the API: the store's own implementations override it.
    #[doc(hidden)]
    fn fn_seq_checked(&self, fn_hash: &str) -> Result<i64> {
        let _ = fn_hash;
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no fn seq here")))
    }

    /// The read of `slice_seq`, not part of the API: the store's own implementations override it.
    #[doc(hidden)]
    fn slice_seq_checked(&self, slice: &Slice) -> Result<i64> {
        let _ = slice;
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no slice seq here")))
    }

    /// The member-set row of `entry`, or `None` (G2, since 0.6.5).
    fn member_set(&self, entry: &str) -> Result<Option<MemberSet>> {
        check_member_entry(entry)?;
        self.member_set_checked(entry)
    }

    /// Writes a slice's definition (G10, since 0.6.5), kept once per slice id (not per date):
    /// `Ok(true)` when inserted, `Ok(false)` when held already.
    fn put_slice_definition(&self, slice: &Slice) -> Result<bool> {
        self.put_slice_definition_checked(slice)
    }

    /// The definition of the slice `slice_id`, or `None` (G10, since 0.6.5).
    fn slice_definition(&self, slice_id: &str) -> Result<Option<Slice>> {
        check_hex(slice_id, 64, "slice_id")?;
        self.slice_definition_checked(slice_id)
    }

    /// The row of `put_member_set`, checked by the caller. Not part of the API: the store's own
    /// implementations override it (a sealed trait, so no other implementation exists).
    #[doc(hidden)]
    fn put_member_set_checked(&self, set: &MemberSet) -> Result<bool> {
        let _ = set;
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no member sets here")))
    }
    /// The row of `member_set`, checked by the caller. Not part of the API.
    #[doc(hidden)]
    fn member_set_checked(&self, entry: &str) -> Result<Option<MemberSet>> {
        let _ = entry;
        Err(Error::Invalid(datumesh_core::Error::new(datumesh_core::ErrorCode::Schema, "$", "no member sets here")))
    }
    /// The write of `put_slice_definition`. Not part of the API.
    #[doc(hidden)]
    fn put_slice_definition_checked(&self, slice: &Slice) -> Result<bool> {
        let _ = slice;
        Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "no slice definitions here",
        )))
    }
    /// The read of `slice_definition`. Not part of the API.
    #[doc(hidden)]
    fn slice_definition_checked(&self, slice_id: &str) -> Result<Option<Slice>> {
        let _ = slice_id;
        Err(Error::Invalid(datumesh_core::Error::new(
            datumesh_core::ErrorCode::Schema,
            "$",
            "no slice definitions here",
        )))
    }
    /// Stores a descriptor (checked and canonicalised by the core); returns its function
    /// hash. Idempotent.
    fn put_descriptor(&self, bytes: &[u8]) -> Result<String>;
    /// Stores a receipt that verifies (SPEC 10.1 steps 1-6) against the key document stored
    /// for its signer host with [`put_namespace`](Store::put_namespace), in canonical form,
    /// with an edge to each of its inputs; returns its hash. An input that is a slice entry
    /// (`slice:<slice id>:<members hash>`, since 0.3.1) is recorded as such
    /// ([`slice_entries`](Store::slice_entries)); its manifest need not be stored, before or
    /// after (it is needed only to recompute, SPEC 10.1 step 7.5). The receipt's cell is
    /// indexed by its typed arguments ([`slice_members`](Store::slice_members)). A receipt that does not verify
    /// is refused with `verify`'s code (no key document for the host: `E_KEY_UNAVAILABLE`).
    /// Idempotent: bytes whose canonical form is the stored copy return its hash without
    /// checking the signature again (it verified when stored), even if the key has since
    /// been revoked. Other bytes with a stored hash are verified first, then refused as
    /// [`Error::Conflict`].
    fn put_receipt(&self, bytes: &[u8]) -> Result<String>;
    /// Stores blob content; returns its SHA-256 (lowercase hex). Idempotent.
    fn put_blob(&self, bytes: &[u8]) -> Result<String>;
    /// Blob content by SHA-256.
    fn blob(&self, sha256: &str) -> Result<Option<Cow<'_, [u8]>>>;
    /// Appends a version: from `tx` on, `cell_id` has the value of receipt `receipt_hash`,
    /// which must be stored, of that cell and at that tx. `tx` must be later than the cell's
    /// latest version ([`Error::NotLater`]).
    fn put_version(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()>;
    /// The cell's latest version with `tx <= asof`.
    fn version_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>>;
    /// The cell's versions with `from <= tx <= to`, in tx order.
    fn versions(&self, cell_id: &str, from: &str, to: &str) -> Result<Vec<Version>>;
    /// The cell's latest version.
    fn latest(&self, cell_id: &str) -> Result<Option<Version>>;
    /// [`latest`](Store::latest) of each of `cell_ids`, in input order (duplicates allowed;
    /// empty input, empty result). The first malformed id is the error. Since 0.4.3.
    fn latest_many(&self, cell_ids: &[&str]) -> Result<Vec<Option<Version>>> {
        cell_ids.iter().map(|id| self.latest(id)).collect()
    }
    /// [`receipt`](Resolver::receipt) of each of `hashes`, in input order, as owned bytes.
    /// Since 0.4.3.
    fn receipts_many(&self, hashes: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
        Ok(hashes.iter().map(|h| self.receipt(h).map(|b| b.into_owned())).collect())
    }
    /// For each item in order, [`put_receipt`](Store::put_receipt) and then, when the item
    /// has a `version`, [`put_version`](Store::put_version); returns the receipt hashes.
    /// All or nothing on PostgreSQL (one transaction; calls of up to 10,000 items are the
    /// size it is written for). SQLite (and this default) loops, stops at the first error,
    /// and need not be atomic: the items before a failing one stay stored there. Since 0.4.3.
    fn put_cells(&self, items: &[CellWrite<'_>]) -> Result<Vec<String>> {
        items
            .iter()
            .map(|w| {
                let hash = self.put_receipt(w.receipt)?;
                if let Some((cell_id, tx)) = w.version {
                    self.put_version(cell_id, tx, &hash)?;
                }
                Ok(hash)
            })
            .collect()
    }
    /// The stored receipts that list `receipt_hash` among their inputs, by hash. Only as
    /// an input of its own: a receipt read as a member of a slice is found through its
    /// manifests ([`member_manifests`](Store::member_manifests), then
    /// [`slice_dependents`](Store::slice_dependents)).
    fn dependents(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The inputs of a stored receipt that are receipt hashes (whether or not they are
    /// stored). Its slice entries are [`slice_entries`](Store::slice_entries).
    fn inputs(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// Records a host's key document, checked as a 200 response for that host would be
    /// (SPEC 9.2, 9.3), validated at `validated_at`. Re-validation replaces the document; a
    /// `validated_at` earlier than the recorded one is [`Error::NotLater`]. Receipts already
    /// stored are not re-checked: they verified when they were stored (and putting one again
    /// stays idempotent).
    fn put_namespace(&self, host: &str, keydoc_bytes: &[u8], validated_at: &str) -> Result<()>;
    /// A host's recorded key document.
    fn namespace(&self, host: &str) -> Result<Option<Namespace>>;
    /// Every recorded namespace, by host. Since the namespace checks (since 0.4.1).
    fn namespaces(&self) -> Result<Vec<Namespace>>;
    /// Records a failed check of `host`'s key document at `checked_at` (domain validation):
    /// the consecutive failures grow by one, and when they reach `threshold` (zero counts as
    /// one) the namespace is suspended at `checked_at`, unless it already is (the first
    /// suspension's time is kept). While it is suspended the store refuses new writes under
    /// the host: `put_receipt` of a receipt signed by it, `put_version` of such a receipt
    /// and `bind_name` of a name under it ([`Error::NamespaceSuspended`]); reads,
    /// verification and the stored key document are not affected, and neither is putting a
    /// receipt that is already stored. Returns the namespace as recorded, or `None` for a
    /// host with no namespace (there is nothing to suspend). A `checked_at` earlier than the
    /// latest check is [`Error::NotLater`]. Since the namespace checks (since 0.4.1).
    fn record_namespace_failure(&self, host: &str, checked_at: &str, threshold: u32) -> Result<Option<Namespace>>;
    /// Records a successful check of `host`'s key document at `checked_at`: the consecutive
    /// failures are reset to zero and a suspension is lifted (the namespace is reinstated).
    /// The document itself is recorded with [`put_namespace`](Store::put_namespace) (which
    /// neither suspends nor reinstates). Returns the namespace as recorded, or `None` for a
    /// host with no namespace. A `checked_at` earlier than the latest check is
    /// [`Error::NotLater`]. Since the namespace checks (since 0.4.1).
    fn record_namespace_success(&self, host: &str, checked_at: &str) -> Result<Option<Namespace>>;
    /// Appends a binding: from `tx` on, the fully qualified function name `fn_name` means
    /// `fn_hash`, whose descriptor must be stored. `tx` must be later than the name's latest
    /// binding ([`Error::NotLater`]).
    fn bind_name(&self, fn_name: &str, fn_hash: &str, tx: &str) -> Result<()>;
    /// The function hash `fn_name` meant as of `asof`: the hash of
    /// [`resolve_binding`](Store::resolve_binding)'s binding.
    fn resolve_name(&self, fn_name: &str, asof: &str) -> Result<Option<String>>;
    /// The binding of `fn_name` that answers as of `asof`: the one with the latest `tx <=
    /// asof`, with its function hash and its `tx`, when it began; `None` before the first.
    /// A name bound again to a hash it meant before (even the hash of the binding before
    /// it) begins again: the later binding's `tx`. Since 0.3.0.
    fn resolve_binding(&self, fn_name: &str, asof: &str) -> Result<Option<Binding>>;

    /// Stores a class descriptor (SPEC 16.1, 16.2) under its class hash and returns the hash.
    /// Since 0.4.1.
    ///
    /// `bytes` must be a **flattened** descriptor in **canonical form** (the bytes
    /// `ClassDescriptor::canonical` gives, 6.2: what the class hash hashes). The core checks it
    /// (`ClassDescriptor::from_json`: strict JSON, then 16.1 in its check order; a flattened
    /// descriptor passes). Bytes that are a valid descriptor but not the canonical form of
    /// it (white space, members out of order) are refused with [`Error::Invalid`], code
    /// `E_SCHEMA`, and nothing is stored: unlike [`put_descriptor`](Store::put_descriptor),
    /// which canonicalises, the bytes put are the bytes stored, so the class hash is the
    /// SHA-256 of what [`class`](Store::class) returns. Idempotent for the same bytes.
    ///
    /// The store does not flatten (that is define's, with the parent in hand), and does not
    /// need the parent, the method functions or the interfaces to be stored (`bind_class`
    /// needs the methods' descriptors). It does not check the owner either (16.2 step 1 is the
    /// definer's check: the store records, http and the engine enforce). It is a write under
    /// the class's steward namespace (`steward.namespace`): refused with
    /// [`Error::NamespaceSuspended`] while that namespace is suspended, even for a class
    /// already stored.
    fn put_class(&self, bytes: &[u8]) -> Result<String>;
    /// The canonical bytes of the class descriptor with this class hash, as stored
    /// ([`put_class`](Store::put_class)); `None` when no such class is stored. A malformed
    /// hash is `E_CANON_HEX`. Never refused under a suspended namespace. Since 0.4.1.
    fn class(&self, class_hash: &str) -> Result<Option<Vec<u8>>>;
    /// Binds the class name `class_name` (a full name, 2.3) to the stored class `class_hash`
    /// from `tx` on and, in the same transaction, binds each method `<class_name>/<m>` of the
    /// stored (flattened, so inherited methods included) descriptor to the method's function
    /// hash from the same `tx` (SPEC 16.3, 9.7). All of it or nothing:
    ///
    /// - the class must be stored ([`Error::NotFound`], `what` `"class"`), and so must every
    ///   method's function descriptor ([`Error::NotFound`], `what` `"descriptor"`, the missing
    ///   function hash; the first in JCS order of method names);
    /// - class names and function names share one namespace of names: `class_name`, or a
    ///   method name, whose latest binding is of the other kind (a function for `class_name`,
    ///   a class for a method name) is refused with [`Error::NameTaken`]. A method name bound
    ///   as a function, to whatever hash, is simply bound again;
    /// - `tx` must be later than the latest binding of `class_name` and of each method name
    ///   ([`Error::NotLater`], `what` `"name"`, the name that is not);
    /// - while the namespace of `class_name`'s host is suspended: [`Error::NamespaceSuspended`].
    ///
    /// The store does not check that the class name is under the steward's namespace, nor
    /// who binds it (the definer's check, 16.2 step 1). Since 0.4.1.
    ///
    /// Since 0.5.4 (K5), only the methods the class adds or overrides are bound under its name:
    /// those whose function hash its parent's flattened descriptor does not give (or that the
    /// parent does not have). An inherited method's name is not bound, so `S/m` of an inherited
    /// `m` answers `None`. Under 0.5.3 every flattened method was bound.
    fn bind_class(&self, class_name: &str, class_hash: &str, tx: &str) -> Result<()>;
    /// Closes the latest binding of `name`, a function name or a class name, from `at` on: as
    /// of `at` or later the name resolves to `None` (also through
    /// [`resolve_binding`](Store::resolve_binding) and [`resolve_class_binding`](Store::resolve_class_binding)),
    /// and as of an earlier instant the binding it had. Closing a class name closes its
    /// method names (`<class name>/<m>`, the ones [`bind_class`](Store::bind_class) bound) from
    /// the same `at`. The name may be bound again later ([`bind_name`](Store::bind_name),
    /// [`bind_class`](Store::bind_class)), with a `tx` after `at`. Each close is one row of the
    /// binding history (with `written_at`). `at` must be later than the latest binding of the
    /// name; a name with no binding, or whose latest binding is closed, is [`Error::NotFound`]
    /// (`what` is `binding`). A malformed instant is `E_CANON_INSTANT`. The default is
    /// [`Error::Unsupported`]. Since 0.6.0.
    fn close_binding(&self, name: &str, at: &str) -> Result<()> {
        let _ = (name, at);
        Err(Error::Unsupported("closing a binding needs a store of 0.6.0".into()))
    }
    /// The class hash `class_name` meant as of `asof`: the hash of
    /// [`resolve_class_binding`](Store::resolve_class_binding)'s binding. `None` before the
    /// first binding, and for a name whose binding as of `asof` is a function's. Since 0.4.1.
    fn resolve_class(&self, class_name: &str, asof: &str) -> Result<Option<String>>;
    /// The class binding of `class_name` that answers as of `asof`: the latest with
    /// `tx <= asof`, with its class hash and its `tx` (when it began; binding the same hash
    /// again begins again), as [`resolve_binding`](Store::resolve_binding) does for functions.
    /// `None` before the first, and for a function name. The function accessors
    /// ([`resolve_name`](Store::resolve_name), [`resolve_binding`](Store::resolve_binding))
    /// answer `None` for a class name: a bindings answer (9.7) has `fn_hash` or `class_hash`,
    /// never both. Since 0.4.1.
    fn resolve_class_binding(&self, class_name: &str, asof: &str) -> Result<Option<ClassBinding>>;
    /// The class names whose class binding as of `at` (the latest with `tx <= at`, as
    /// [`resolve_class_binding`](Store::resolve_class_binding) takes it) is `class_hash`, in byte
    /// order, each once. A name rebound to another hash is no longer listed from that binding's
    /// `tx` on. Function names are never listed. Empty when none is bound (also when the hash is
    /// not stored, or `at` is before the first binding). A malformed hash is `E_CANON_HEX`, a
    /// malformed instant `E_CANON_INSTANT`. The default is [`Error::Unsupported`]. Since 0.5.3.
    fn class_names_bound_to(&self, class_hash: &str, at: &str) -> Result<Vec<String>> {
        let _ = (class_hash, at);
        Err(Error::Unsupported("class names need a store of 0.5.3".into()))
    }

    /// Records that `receipt_hash` (stored, a receipt of `cell_id` at `tx`) is a computed
    /// version of the cell. Insert-only, in any order (an older version may be added after a
    /// newer one): `tx` need not be later than anything. The version's asof is the
    /// receipt's. Idempotent for the same (cell_id, tx, asof, receipt_hash); another receipt
    /// for an existing (cell_id, tx, asof) is [`Error::Conflict`] (its `hash` is the receipt
    /// refused) and the stored one is kept. The receipt is checked as for
    /// [`put_version`](Store::put_version): [`Error::NotFound`] when it is not stored,
    /// [`Error::Mismatch`] when it is of another cell or at another tx. Since 0.2.0.
    fn put_computed(&self, cell_id: &str, tx: &str, receipt_hash: &str) -> Result<()>;
    /// The computed version that answers as of `asof`: the one with the greatest asof <=
    /// `asof` (between equal asof, the greatest tx), or `None`. Since 0.2.0.
    fn computed_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>>;
    /// The stored version that answers as of `asof`: the latest version (by tx) with `tx <=
    /// asof` whose receipt's asof is also `<= asof`, or `None`. A version whose receipt is
    /// late-signed (asof later than tx) answers only from its asof on; until then the
    /// version before it answers. ([`version_asof`](Store::version_asof) keeps its tx-only
    /// meaning.) Since 0.2.0.
    fn stored_asof(&self, cell_id: &str, asof: &str) -> Result<Option<Version>>;
    /// The cells that read `cell_id`: the cell ids of the stored receipts that list, among
    /// their inputs, any stored receipt of `cell_id` (lineage: edges joined to receipts),
    /// or (since 0.3.1) a slice entry whose stored manifest lists one as a member.
    /// Ascending, each once. Since 0.2.0.
    fn cell_dependents(&self, cell_id: &str) -> Result<Vec<String>>;

    /// Stores a manifest (SPEC 15.13), `{"members": [...], "slice": {...}}`: parsed strictly
    /// and checked by the core (`Manifest::from_json`: structure, members, their order and
    /// limit, the slice object), kept in canonical form under its entry
    /// `slice:<slice id>:<members hash>`, which it returns, and recorded for each member
    /// ([`member_manifests`](Store::member_manifests)). The members need not be stored.
    /// Idempotent; insert-only (the entry is a hash of the content, so a stored entry
    /// always has these bytes). Served by [`manifest`](Resolver::manifest). Since 0.3.1.
    fn put_manifest(&self, bytes: &[u8]) -> Result<String>;
    /// The entries of the stored manifests that list `receipt_hash` among their members,
    /// ascending: from a member, the slices it was read in (then
    /// [`slice_dependents`](Store::slice_dependents)). Since 0.3.1.
    fn member_manifests(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The slice entries among the inputs of a stored receipt, ascending. Since 0.3.1.
    fn slice_entries(&self, receipt_hash: &str) -> Result<Vec<String>>;
    /// The stored receipts whose inputs list the slice entry `entry`, by hash, ascending.
    /// An entry is `slice:` and two 64-digit hashes (`E_CANON_HEX` otherwise). Since 0.3.1.
    fn slice_dependents(&self, entry: &str) -> Result<Vec<String>>;
    /// The members of `slice` as of `asof` (SPEC 15.13), with the stored version of each
    /// that answers as of `asof` and its receipt's bytes.
    ///
    /// For a slice of cells: the cells of the slice's function (its `fn_hash`, `ns` and
    /// `fn`, with the same argument names) whose bound arguments are the same typed scalar,
    /// whose ranged arguments are of the range's type and within it (`lo <= v <= hi`,
    /// inclusive, in the type's order: numbers as exact decimals, dates and instants
    /// chronologically, strings by code point; `lo` after `hi` is empty), any value for an
    /// open argument, and that have a stored version answering as of `asof`: as
    /// [`stored_asof`](Store::stored_asof), the latest version with `tx <= asof` whose
    /// receipt's `asof <= asof` (so a late-signed version is not a member's answer before
    /// its asof, and a correction replaces a member's version from its asof on). Computed
    /// versions are not members. In key order: by the open and ranged arguments in
    /// ascending name order, each in its type's order (`false` before `true`; blobs as
    /// text); the cell id last, for two cells whose key arguments are of different types.
    ///
    /// For a version slice `{"cell", "from", "to"}`: [`versions_between`](Store::versions_between)
    /// of its cell. Empty when nothing matches. Since 0.3.1.
    fn slice_members(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceMember>>;
    /// The stored versions of `cell_id` with `from <= tx <= to` that answer as of `asof`
    /// (`tx <= asof` and the receipt's `asof <= asof`), ascending by tx, each with its
    /// receipt's bytes: SPEC 15.13's version slice. Empty when `from` is after `to`. Since
    /// 0.3.1.
    fn versions_between(&self, cell_id: &str, from: &str, to: &str, asof: &str) -> Result<Vec<SliceMember>>;
    /// The members [`slice_members`](Store::slice_members) gives, in its order and by its
    /// answering rule, each as its version, its receipt's typed value and its cell (with its
    /// arguments), without the receipt's bytes (the value is a stored column of the receipt
    /// and the cell comes from the typed-argument index, neither parsed per call).
    /// Since 0.3.2.
    fn slice_values(&self, slice: &Slice, asof: &str) -> Result<Vec<SliceValue>>;
    /// Whether the members of `slice`, or their versions, may answer differently as of some
    /// time after `asof`: whether a cell matching the slice has a stored version whose
    /// receipt's `asof` is after `asof` (its `tx` is never later than that), for a version
    /// slice one with `tx` in `[from, to]`. `false` means [`slice_members`](Store::slice_members)
    /// (and `slice_values`) give the same answer as of every time from `asof` on, until a
    /// version is stored with an asof after it; `true` may be conservative (a late-signed
    /// version superseded before its asof never answers). Index scans only, whatever the
    /// slice's size: two bounded probes (at most 10,001 entries each) choose whether to walk
    /// the function's receipts with an asof after `asof` or the slice's candidate cells,
    /// whichever is fewer, and one query walks it, stopping at the first match. Since 0.3.2.
    fn slice_changed_after(&self, slice: &Slice, asof: &str) -> Result<bool>;
    /// The latest withdrawal among the cells that match `slice` (SPEC 8.2, spec-0.7.0): among
    /// the cells whose version answering as of `asof` (the rule of `stored_asof`) is a
    /// withdrawal, the greatest by `tx`, then by `asof`; its `(tx, asof)`. `None` when no such
    /// cell. A version slice's one cell counts when its answering version is a withdrawal with
    /// a `tx` in the slice's range. A malformed instant is `E_CANON_INSTANT`. The default is
    /// [`Error::Unsupported`]. Since 0.6.0.
    fn slice_withdrawn_latest(&self, slice: &Slice, asof: &str) -> Result<Option<(String, String)>> {
        let _ = (slice, asof);
        Err(Error::Unsupported("the withdrawn cells of a slice need a store of 0.6.0".into()))
    }
    /// The cells of `slice` (matched as [`slice_values`](Store::slice_values) matches them: the
    /// fixed arguments, `fn_hash`, `ns`, `fn` and the number of arguments) that have a computed
    /// version answering as of `asof`: one with `tx <= asof` whose receipt's asof is `<= asof`
    /// (the rule of [`computed_asof`](Store::computed_asof)). Ascending by cell id, at most
    /// `limit` (`slice_values` takes no limit), each with its arguments from the typed-argument
    /// index. A cell with only a stored version is not returned. Since 0.4.2.
    fn computed_cells(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<Cell>>;
    /// Whether any name binding or class binding has a name starting with `namespace` and `/`
    /// (so `"a.example/n"` is not a namespace of `"a.example/ns/f"`). Since 0.4.2.
    fn has_bindings_under(&self, namespace: &str) -> Result<bool>;

    /// Inserts or replaces catalogue entries, keyed by (kind, name); returns each entry's id,
    /// in order. All or nothing: every entry is checked first (the name by the core's rules,
    /// `hash` as 64 hex digits, the bounds of [`CatalogEntry`]) and refused with
    /// [`Error::Invalid`] before anything is written. An operator's entry never replaces a
    /// publisher's: that entry is kept, and the call still answers its id. A publisher's
    /// replaces either. Replacing an entry's label or description drops its embedding.
    /// Since 0.5.0.
    fn upsert_catalog(&self, entries: &[CatalogEntry]) -> Result<Vec<i64>>;
    /// Inserts or replaces the values of `key` of `owner` (a function or class name): each
    /// value's label and aliases are replaced, and its `n_cells` is kept. Values not in
    /// `values` stay (an upsert adds and replaces, it does not prune). Returns each value's
    /// id, in order. Replacing a value's label drops its embedding. Since 0.5.0.
    fn upsert_key_values(&self, owner: &str, key: &str, values: &[KeyValue]) -> Result<Vec<i64>>;
    /// Sets every dictionary value's `n_cells` to the number of cells of the function named
    /// by its owner that have that value for the key; returns the number of rows refreshed
    /// (every value, changed or not). Since 0.5.0.
    fn refresh_value_counts(&self) -> Result<u64>;
    /// The entries and dictionary values that match every word of `query` (split on white
    /// space; a word matches case-insensitively, literally, in any field of the entry or
    /// value, and scores 1.0 as a whole field, 0.8 as one of a field's words, 0.5 inside a
    /// field; a score is the mean over the words, in (0, 1]). Best first, ties by name; at
    /// most `limit`. A blank query finds nothing. Since 0.5.0.
    fn text_find(&self, query: &str, limit: usize) -> Result<Vec<Hit>>;
    /// The entries and values nearest to `vec` by cosine similarity, among those with an
    /// embedding for `model`: each score is 1 minus the cosine distance (1.0 for the same
    /// direction), best first, at most `limit`. `vec` has 384 finite values, not all zero
    /// ([`Error::Invalid`] otherwise). [`Error::Unsupported`] where
    /// [`vector_available`](Store::vector_available) is false. Since 0.5.0.
    fn vector_find(&self, model: &str, vec: &[f32], limit: usize) -> Result<Vec<Hit>>;
    /// Stores embeddings for `model`, one per (kind, id, model): an existing one is replaced.
    /// All or nothing; a reference to an entry or value that is not stored is
    /// [`Error::NotFound`], and a vector is checked as [`vector_find`](Store::vector_find)
    /// checks it. [`Error::Unsupported`] where `vector_available` is false. Since 0.5.0.
    fn put_embeddings(&self, model: &str, items: &[Embedding]) -> Result<()>;
    /// Up to `limit` entries and values with no embedding for `model`, with the text to embed
    /// (see [`PendingEmbedding`]). Empty where `vector_available` is false (nothing can be
    /// stored there). Since 0.5.0.
    fn pending_embeddings(&self, model: &str, limit: usize) -> Result<Vec<PendingEmbedding>>;
    /// Whether vectors can be stored and searched by this store: PostgreSQL with the `vector`
    /// extension, where the migration made the `embeddings` table. Looked at when the store
    /// is opened (migrate before opening, as the schema is then in place). Since 0.5.0.
    fn vector_available(&self) -> bool;
    /// Records a find that found nothing: the query's text (trimmed, at most 512 bytes; a
    /// blank query records nothing) and the time. The store keeps at most
    /// [`FIND_MISSES_CAP`] rows, pruning the oldest. Since 0.5.0.
    fn log_miss(&self, query: &str) -> Result<()>;
    /// The catalogue as an operator reads it: every entry with its label and its cell count,
    /// and every dictionary with its first 50 values (by value) and how many it has. Since 0.5.0.
    fn catalog_summary(&self) -> Result<Summary>;

    /// Stores a function test, if its (fn_hash, case) has none: `Ok(true)` when the row is
    /// inserted, `Ok(false)` when one exists, in which case nothing changes (the first
    /// writer's `signed_by` and `test_json` stay). The test is checked first: a 64-digit
    /// hash, a 16-digit case, a canonical `test_json`, a signer of 1 to 256 bytes, an instant
    /// and result if it carries them, and branch counts with covered not above total, each
    /// at most `i32::MAX`; a failing check is [`Error::Invalid`] and nothing is stored.
    /// Since 0.5.1.
    fn put_function_test(&self, test: &FunctionTest) -> Result<bool>;
    /// Every test of `fn_hash`, ordered by case ascending; empty when there are none. Since 0.5.1.
    fn function_tests(&self, fn_hash: &str) -> Result<Vec<FunctionTest>>;
    /// Records a run of the test (`fn_hash`, `case`): sets its `last_run_at` (the instant
    /// `run_at`), `last_result` and `last_detail`. `Ok(true)` when the test exists; `Ok(false)`
    /// when it does not, and nothing is inserted. `result` must be `pass`, `fail` or `error`:
    /// any other value is [`Error::Invalid`], and nothing changes. Since 0.5.1.
    fn record_function_test(&self, fn_hash: &str, case: &str, run_at: &str, result: &str, detail: &str)
        -> Result<bool>;
    /// Sets `branches_total` and `branches_covered` on every test of `fn_hash`. A function
    /// with no tests is a no-op, not an error. `covered` above `total` is [`Error::Invalid`].
    /// Since 0.5.1.
    fn set_function_branches(&self, fn_hash: &str, total: u32, covered: u32) -> Result<()>;
    /// The function hashes of the stored descriptors whose `inputs` contain `name` (a fully
    /// qualified function name), distinct, ascending. Answered from the index of descriptor
    /// inputs, which [`put_descriptor`](Store::put_descriptor) fills; the migration that
    /// introduced the index filled it from the descriptors stored before. Since 0.5.1.
    fn function_readers(&self, name: &str) -> Result<Vec<String>>;
    /// Every fully qualified name ever bound (by [`bind_name`](Store::bind_name)) to
    /// `fn_hash`, distinct, ascending. A name since bound to another hash still lists this
    /// one. Since 0.5.1.
    fn bound_names(&self, fn_hash: &str) -> Result<Vec<String>>;

    /// Sets the function name of tests of `fn_hash`: each `(case, fn_name)` names the test of
    /// that case, if it has no name yet. A name once set is never changed, and a case with no
    /// test changes nothing. Returns the number of tests named (rows changed); an empty list
    /// is 0. The default is [`Error::Unsupported`]. Since 0.5.2.
    fn name_function_tests(&self, fn_hash: &str, names: &[(String, String)]) -> Result<u64> {
        let _ = (fn_hash, names);
        Err(Error::Unsupported("naming function tests needs a store of 0.5.2".into()))
    }
    /// Up to `limit` tests that have no function name, ordered by (function hash, case), each
    /// as [`function_tests`](Store::function_tests) gives it. Naming them drains the list. The
    /// default is [`Error::Unsupported`]. Since 0.5.2.
    fn unnamed_function_tests(&self, limit: usize) -> Result<Vec<FunctionTest>> {
        let _ = limit;
        Err(Error::Unsupported("unnamed tests need a store of 0.5.2".into()))
    }
    /// The summary of the tests of `fn_hash` in one aggregate: every test of the hash when
    /// `fn_name` is `None`, else only those named `fn_name`. A hash with no such test gives the
    /// zero summary ([`TestSummary::default`]). The default computes it from
    /// [`function_tests`](Store::function_tests) when `fn_name` is `None`, and is
    /// [`Error::Unsupported`] for a name (0.5.1 does not give a test's name). Since 0.5.2.
    fn function_test_summary(&self, fn_hash: &str, fn_name: Option<&str>) -> Result<TestSummary> {
        if fn_name.is_some() {
            return Err(Error::Unsupported("a summary by function name needs a store of 0.5.2".into()));
        }
        let mut s = TestSummary::default();
        for t in self.function_tests(fn_hash)? {
            s.count += 1;
            match t.last_result.as_deref() {
                Some("pass") => s.pass += 1,
                Some("fail") => s.fail += 1,
                Some("error") => s.error += 1,
                _ => s.not_run += 1,
            }
            if let Some(at) = t.last_run_at {
                if s.last_run_at.as_ref().is_none_or(|latest| at > *latest) {
                    s.last_run_at = Some(at);
                }
            }
            s.branches_total = s.branches_total.max(t.branches_total);
            s.branches_covered = s.branches_covered.max(t.branches_covered);
        }
        Ok(s)
    }
    /// A page of the tests of `fn_hash` by case ascending: those named `fn_name` (or all, when
    /// `None`), with a case after `after_case` (exclusive, when given), at most `limit`.
    /// Whole rows, as [`function_tests`](Store::function_tests) gives them. The default is the
    /// page of [`function_tests`](Store::function_tests) when `fn_name` is `None`, and is
    /// [`Error::Unsupported`] for a name. Since 0.5.2.
    fn function_tests_page(
        &self,
        fn_hash: &str,
        fn_name: Option<&str>,
        after_case: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FunctionTest>> {
        if fn_name.is_some() {
            return Err(Error::Unsupported("a page by function name needs a store of 0.5.2".into()));
        }
        Ok(self
            .function_tests(fn_hash)?
            .into_iter()
            .filter(|t| after_case.is_none_or(|after| t.case.as_str() > after))
            .take(limit)
            .collect())
    }
    /// The receipts (bytes, as [`receipt`](Resolver::receipt) returns them) of the latest
    /// stored version of up to `limit` cells of `ns`/`fn_name`/`fn_hash` that have one, lowest
    /// cell id first: at most `limit` cells are read. The default is [`Error::Unsupported`]
    /// (0.5.1 has no read of a function's cells without a slice). Since 0.5.2.
    fn stored_receipts_of(&self, ns: &str, fn_name: &str, fn_hash: &str, limit: usize) -> Result<Vec<Vec<u8>>> {
        let _ = (ns, fn_name, fn_hash, limit);
        Err(Error::Unsupported("stored receipts of a function need a store of 0.5.2".into()))
    }
    /// [`slice_values`](Store::slice_values) with at most `limit` members: the first `limit` of
    /// its order (the same members, in the same order). The default takes them from
    /// `slice_values`. Since 0.5.2.
    fn slice_values_limit(&self, slice: &Slice, asof: &str, limit: usize) -> Result<Vec<SliceValue>> {
        let mut values = self.slice_values(slice, asof)?;
        values.truncate(limit);
        Ok(values)
    }
    /// [`versions`](Store::versions) with at most `limit` versions: the first `limit`, in tx
    /// order. The default takes them from `versions`. Since 0.5.2.
    fn versions_limit(&self, cell_id: &str, from: &str, to: &str, limit: usize) -> Result<Vec<Version>> {
        let mut versions = self.versions(cell_id, from, to)?;
        versions.truncate(limit);
        Ok(versions)
    }
    /// The number of cells of `ns`/`fn_name`/`fn_hash` that have a stored version (a second
    /// version of a cell is not a second cell). The default is [`Error::Unsupported`]. Since
    /// 0.5.2.
    fn stored_cell_count(&self, ns: &str, fn_name: &str, fn_hash: &str) -> Result<u64> {
        let _ = (ns, fn_name, fn_hash);
        Err(Error::Unsupported("counting a function's cells needs a store of 0.5.2".into()))
    }
    /// Whether `owner` has the dictionary value `value` for `key`, exactly (the value's text,
    /// case-sensitive; no prefix, substring or label match). Indexed in the native stores. The
    /// default asks [`text_find`](Store::text_find) for `value` and keeps the exact match.
    /// Since 0.5.2.
    fn has_key_value(&self, owner: &str, key: &str, value: &str) -> Result<bool> {
        Ok(self.text_find(value, usize::MAX)?.iter().any(
            |h| matches!(h, Hit::Value { owner: o, key: k, value: v, .. } if o == owner && k == key && v == value),
        ))
    }
    /// The number of dictionary values `owner` has for `key`. The default is
    /// [`Error::Unsupported`]. Since 0.5.2.
    fn key_value_count(&self, owner: &str, key: &str) -> Result<u64> {
        let _ = (owner, key);
        Err(Error::Unsupported("counting a dictionary needs a store of 0.5.2".into()))
    }
    /// Marks the key `key` of `owner` open: it takes no automatic values from now on. Marking
    /// an open key again changes nothing. Its existing values stay. The default is
    /// [`Error::Unsupported`]. Since 0.5.2.
    fn mark_key_open(&self, owner: &str, key: &str) -> Result<()> {
        let _ = (owner, key);
        Err(Error::Unsupported("open keys need a store of 0.5.2".into()))
    }
    /// Whether the key `key` of `owner` has been marked open ([`mark_key_open`](Store::mark_key_open)).
    /// The default is [`Error::Unsupported`]. Since 0.5.2.
    fn key_is_open(&self, owner: &str, key: &str) -> Result<bool> {
        let _ = (owner, key);
        Err(Error::Unsupported("open keys need a store of 0.5.2".into()))
    }
    /// The number of keys marked open, of any owner ([`mark_key_open`](Store::mark_key_open)).
    /// The default is 0, which is exact for a store with no open key. Since 0.5.2.
    fn open_key_count(&self) -> Result<u64> {
        Ok(0)
    }
    /// The catalogue entry named `name` (exactly), if there is one. A name may be the name of a
    /// function and of a class; the function's entry is given first, then a method's, then a
    /// class's. The default is [`Error::Unsupported`]. Since 0.5.2.
    fn catalog_entry(&self, name: &str) -> Result<Option<CatalogEntry>> {
        let _ = name;
        Err(Error::Unsupported("an exact catalogue lookup needs a store of 0.5.2".into()))
    }

    /// Inserts or replaces catalogue entries with their facets and `extends`, keyed by (kind,
    /// name), as [`upsert_catalog`](Store::upsert_catalog) does, and replaces the facets and
    /// `extends` of an existing row. Checked first, all or nothing ([`Error::Invalid`]): the
    /// facets (a key of domain, thing, measure, region or frequency; 1 to 8 names each, names of
    /// at most 32 bytes, ascending and unique), and `extends` (a full function name, on a
    /// function only). The store does not check that `extends` names a stored class, nor the key
    /// rule of 16.3. Returns each entry's id, in order. The default is [`Error::Unsupported`].
    /// Since 0.6.0.
    fn upsert_catalog_ext(&self, entries: &[CatalogEntryExt]) -> Result<Vec<i64>> {
        let _ = entries;
        Err(Error::Unsupported("catalogue facets need a store of 0.5.3".into()))
    }
    /// The catalogue entry of kind `kind` and name `name` (exactly), with its facets and
    /// `extends`, if there is one. The default is [`Error::Unsupported`]. Since 0.6.0.
    fn catalog_entry_ext(&self, kind: CatalogKind, name: &str) -> Result<Option<CatalogEntryExt>> {
        let _ = (kind, name);
        Err(Error::Unsupported("catalogue facets need a store of 0.5.3".into()))
    }
    /// The entries that have at least one facet, by kind (as text: class, function, method)
    /// then name in byte order, each with its first binding time (see [`FacetedEntry`]).
    /// The default is [`Error::Unsupported`]. Since 0.6.0.
    fn catalog_entries_with_facets(&self) -> Result<Vec<FacetedEntry>> {
        Err(Error::Unsupported("catalogue facets need a store of 0.5.3".into()))
    }
    /// The function entries whose `extends` is `class`, by name in byte order. A `class` that
    /// is not a full name is [`Error::Invalid`]. The default is [`Error::Unsupported`]. Since 0.6.0.
    fn extensions_of(&self, class: &str) -> Result<Vec<CatalogEntryExt>> {
        let _ = class;
        Err(Error::Unsupported("catalogue facets need a store of 0.5.3".into()))
    }
    /// [`extensions_of`](Store::extensions_of) with at most `limit` entries: the first `limit` by name (CONTRACT-round-0.7 R11,
    /// the bounded read of a describe). The backends read `limit` rows (`LIMIT`, with the index on `extends`). The default takes
    /// them from `extensions_of`. Since 0.6.0.
    fn extensions_of_limit(&self, class: &str, limit: usize) -> Result<Vec<CatalogEntryExt>> {
        let mut all = self.extensions_of(class)?;
        all.truncate(limit);
        Ok(all)
    }

    /// The number of cells of `ns`/`fn_name`/`fn_hash`: every cell with a stored receipt, a
    /// second version of a cell not counted again. Read from the counter (PK), not counted
    /// over the cells. The default is [`stored_cell_count`](Store::stored_cell_count). Since
    /// 0.5.3.
    fn cell_count(&self, ns: &str, fn_name: &str, fn_hash: &str) -> Result<u64> {
        self.stored_cell_count(ns, fn_name, fn_hash)
    }
    /// The test of `fn_hash` with case `case` (16 hex digits), if there is one: one row by
    /// primary key, as [`function_tests`](Store::function_tests) gives it. The default takes
    /// it from [`function_tests`](Store::function_tests). Since 0.5.3.
    fn function_test(&self, fn_hash: &str, case: &str) -> Result<Option<FunctionTest>> {
        Ok(self.function_tests(fn_hash)?.into_iter().find(|t| t.case == case))
    }
    /// Up to `limit` tests of `fn_hash` whose last result is `fail` or `error`, by case
    /// ascending, among those named `fn_name` (or all, when `None`). A SQL limit: the rows
    /// past it are not read. The default pages [`function_tests_page`](Store::function_tests_page)
    /// (which is [`Error::Unsupported`] for a name). Since 0.5.3.
    fn function_tests_failing(&self, fn_hash: &str, fn_name: Option<&str>, limit: usize) -> Result<Vec<FunctionTest>> {
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        let mut after: Option<String> = None;
        loop {
            let page = self.function_tests_page(fn_hash, fn_name, after.as_deref(), 256)?;
            let Some(last) = page.last() else { return Ok(out) };
            after = Some(last.case.clone());
            for t in page {
                if matches!(t.last_result.as_deref(), Some("fail") | Some("error")) {
                    out.push(t);
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
        }
    }
    /// The stored `test_case` cells of any namespace whose `function_hash` argument is
    /// `fn_hash` and whose latest version is `false`: each as (cell id, the bytes of the
    /// receipt of that latest version), at most `limit`, in one query. The default is
    /// [`Error::Unsupported`]. Since 0.5.3.
    fn false_test_cells(&self, fn_hash: &str, limit: usize) -> Result<Vec<(String, Vec<u8>)>> {
        let _ = (fn_hash, limit);
        Err(Error::Unsupported("false test cells need a store of 0.5.3".into()))
    }
    /// The names bound at `at` to a function hash that reads `name` (its descriptor's inputs
    /// name it), as (name, fn_hash), at most `limit`. The default walks
    /// [`function_readers`](Store::function_readers), [`bound_names`](Store::bound_names) and
    /// [`resolve_name`](Store::resolve_name). Since 0.5.3.
    fn dependants_of(&self, name: &str, at: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        for h in self.function_readers(name)? {
            for n in self.bound_names(&h)? {
                if self.resolve_name(&n, at)?.as_deref() == Some(h.as_str()) {
                    out.push((n, h.clone()));
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }
    /// A page of the members of `slice` as of `asof` in sort-key order (CONTRACT-0.5.1 S2):
    /// those whose [`SliceValue::sort_key`] is greater than `after` (exclusive; all of them
    /// when `None`), at most `limit`, with the same membership as
    /// [`slice_values`](Store::slice_values) (fixed arguments, ranges, the latest version as of
    /// `asof`, withdrawals left out), so that a walk of pages, each from the last member's key,
    /// gives `slice_values` member for member. A version slice has no key order to page by:
    /// [`Error::Unsupported`]. The default takes all of `slice_values` and pages them; the
    /// stores read the index of the function's cells. Since 0.6.4.
    fn slice_page(&self, slice: &Slice, asof: &str, after: Option<&[u8]>, limit: usize) -> Result<Vec<SliceValue>> {
        if slice.is_version_slice() {
            return Err(Error::Unsupported("a version slice has no key order to page by".into()));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let values = self.slice_values(slice, asof)?;
        Ok(values.into_iter().filter(|v| after.is_none_or(|a| v.sort_key.as_slice() > a)).take(limit).collect())
    }
    /// A page of the members of `slice` as of `asof`, by cell id ascending: those whose cell
    /// id is after `after_cell_id` (exclusive, when given), at most `limit`, with one value
    /// per member. The default takes all of [`slice_values`](Store::slice_values) and pages
    /// them. Since 0.5.3.
    fn slice_values_page(
        &self,
        slice: &Slice,
        asof: &str,
        after_cell_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SliceValue>> {
        let mut values = self.slice_values(slice, asof)?;
        values.retain(|v| after_cell_id.is_none_or(|a| v.version.cell_id.as_str() > a));
        values.sort_by(|a, b| a.version.cell_id.cmp(&b.version.cell_id));
        values.truncate(limit);
        Ok(values)
    }
    /// Up to `limit` catalogue entries as (kind, name, label, description), with no counts.
    /// The default is [`Error::Unsupported`]. Since 0.5.3.
    fn catalog_entries_all(&self, limit: usize) -> Result<Vec<(CatalogKind, String, String, String)>> {
        let _ = limit;
        Err(Error::Unsupported("the catalogue's entries need a store of 0.5.3".into()))
    }
    /// The values of dictionary `key` of `owner`, in insertion order, at most `limit`. The
    /// default is [`Error::Unsupported`]. Since 0.5.3.
    fn dictionary_values_ordered(&self, owner: &str, key: &str, limit: usize) -> Result<Vec<String>> {
        let _ = (owner, key, limit);
        Err(Error::Unsupported("ordered dictionary values need a store of 0.5.3".into()))
    }
    /// [`computed_asof`](Store::computed_asof) and the bytes of the version's receipt (as
    /// [`receipt`](Resolver::receipt) returns them), which is what a caller reads next.
    /// The default asks for the two in turn (a receipt that cannot be read then is
    /// [`Error::NotFound`]); [`PgStore`] and [`SqliteStore`] answer both with one query.
    /// Since 0.2.0.
    fn computed_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        match self.computed_asof(cell_id, asof)? {
            Some(v) => with_receipt(self, v).map(Some),
            None => Ok(None),
        }
    }
    /// [`stored_asof`](Store::stored_asof) and the bytes of the version's receipt, as
    /// [`computed_asof_with_receipt`](Store::computed_asof_with_receipt) does. Since 0.2.0.
    fn stored_asof_with_receipt(&self, cell_id: &str, asof: &str) -> Result<Option<(Version, Cow<'_, [u8]>)>> {
        match self.stored_asof(cell_id, asof)? {
            Some(v) => with_receipt(self, v).map(Some),
            None => Ok(None),
        }
    }

    // ---- Listings (0.7.0, CONTRACT-http-listing L3; see the `listings` module) -------------------

    /// Writes the listing `l`, all or nothing. Checked first ([`Error::Invalid`]): `host` a DNS name;
    /// `name` a function name whose first segment is `host`; `fn_hash` 64 lowercase hex digits and
    /// the hash of `descriptor` as [`put_descriptor`](Store::put_descriptor) computes it (a
    /// descriptor that does not parse, or hashes to another, is refused); `subjects` 1 to
    /// [`LISTING_SUBJECTS_MAX`], ascending and unique, each key a name of at most 32 bytes;
    /// `key_id` 1 to 256 bytes; `label` at most 120 bytes; `description` 1 to 1024 bytes;
    /// `fetched_at` and `listed_at` instants.
    ///
    /// When the stored row of `name` equals `l` in every field but `fetched_at` and `listed_at`,
    /// nothing is written ([`ListingWrite::Unchanged`]). Otherwise the row is inserted
    /// ([`ListingWrite::Inserted`]) or replaced ([`ListingWrite::Changed`], keeping the stored
    /// `listed_at`), and in the same transaction: the descriptor is put in `descriptors`; the
    /// catalogue entry (kind function, `name`, hash `fn_hash`, `label`, `description`, no args, no
    /// examples, source [`CatalogSource::Listed`], `signed_by` = `host`, facets = each subject
    /// key's distinct values, ascending, at most 8 per key) is upserted, its embedding dropped when
    /// its text changes (as [`upsert_catalog_ext`](Store::upsert_catalog_ext) does). Never touched:
    /// `name_bindings`, the namespace checks, `cells`, `receipts`, `versions`. The default is
    /// [`Error::Unsupported`]. Since 0.7.0.
    fn put_listing(&self, l: &Listing) -> Result<ListingWrite> {
        let _ = l;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// The listing of `name`, if there is one. The default is [`Error::Unsupported`]. Since 0.7.0.
    fn listing(&self, name: &str) -> Result<Option<Listing>> {
        let _ = name;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// The listings of `host`, by name in byte order. The default is [`Error::Unsupported`]. Since 0.7.0.
    fn listings_of(&self, host: &str) -> Result<Vec<Listing>> {
        let _ = host;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// The number of listings of every host. The default is [`Error::Unsupported`]. Since 0.7.0.
    fn listing_count(&self) -> Result<u64> {
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// Removes the listing of `name` and its catalogue entry of source listed (with the entry's
    /// embeddings), in one transaction; the descriptor stays in `descriptors`. `Ok(false)` when there
    /// was none. The default is [`Error::Unsupported`]. Since 0.7.0.
    fn delete_listing(&self, name: &str) -> Result<bool> {
        let _ = name;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// Writes the row of a listed host. Checked first ([`Error::Invalid`]): `host` a DNS name,
    /// `last_seen` a date, `unreachable_since` an instant. When the stored row equals `h`, nothing
    /// is written and the answer is `Ok(false)`; otherwise it is inserted or replaced, `Ok(true)`.
    /// The default is [`Error::Unsupported`]. Since 0.7.0.
    fn put_listed_host(&self, h: &ListedHost) -> Result<bool> {
        let _ = h;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// The row of the listed host `host`, if there is one. The default is [`Error::Unsupported`].
    /// Since 0.7.0.
    fn listed_host(&self, host: &str) -> Result<Option<ListedHost>> {
        let _ = host;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// Every listed host's row, by host in byte order. The default is [`Error::Unsupported`].
    /// Since 0.7.0.
    fn listed_hosts(&self) -> Result<Vec<ListedHost>> {
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// Removes the row of `host` and every listing of `host` (as [`delete_listing`](Store::delete_listing)
    /// does), in one transaction. `Ok(false)` when there was no host row. The default is
    /// [`Error::Unsupported`]. Since 0.7.0.
    fn delete_listed_host(&self, host: &str) -> Result<bool> {
        let _ = host;
        Err(Error::Unsupported("listings need a store of 0.7.0".into()))
    }
    /// Sets (`Some(instant)`) or clears (`None`) the instant the listing of `name` was first not reached by a run that
    /// read its host's catalogue (CONTRACT-http-listing, the re-review's N1: a name's own 7-day clock). Checked first: `since`
    /// an instant ([`Error::Invalid`]). No listing of `name`: [`Error::NotFound`]. When the stored value equals `since`,
    /// nothing is written and the answer is `Ok(false)`; otherwise it is written, `Ok(true)`. It touches no other column
    /// (not `fetched_at`). A deleted listing takes it with it; a new listing of the name starts with none. The default is
    /// [`Error::Unsupported`]. Since 0.7.2.
    fn set_listing_unreached(&self, name: &str, since: Option<&str>) -> Result<bool> {
        let _ = (name, since);
        Err(Error::Unsupported("a listing's unreached clock needs a store of 0.7.2".into()))
    }
    /// The instant the listing of `name` was first not reached, if it is not reached now; `None` when it is, or when there
    /// is no listing of `name`. The default is [`Error::Unsupported`]. Since 0.7.2.
    fn listing_unreached(&self, name: &str) -> Result<Option<String>> {
        let _ = name;
        Err(Error::Unsupported("a listing's unreached clock needs a store of 0.7.2".into()))
    }
}

/// The seal of [`Store`]: a public trait in a private module, so no other crate can name it,
/// and so none can implement `Store`.
mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::PgStore {}
    #[cfg(feature = "sqlite")]
    impl Sealed for crate::SqliteStore {}
}

/// A receipt as the store reads it when it is put (crate-internal, since 0.6.0): a valued
/// receipt, or a withdrawal (8.1), which has no value.
pub(crate) struct Parsed {
    /// The canonical (JCS) bytes, `sig` included: what the store keeps.
    pub(crate) canonical: Vec<u8>,
    pub(crate) hash: String,
    pub(crate) cell_id: String,
    pub(crate) tx: String,
    pub(crate) asof: String,
    pub(crate) fn_hash: String,
    pub(crate) publisher: String,
    /// The receipt's cell (its arguments are indexed by the SQLite store).
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) cell: Cell,
    /// The signer's host (the namespace whose keys verify it).
    pub(crate) host: String,
    /// The slice entries (`slice:` inputs) and the receipt inputs (edges), in the receipt's order.
    pub(crate) entries: Vec<String>,
    pub(crate) inputs: Vec<String>,
    /// The value's type and lexical form; `None` for a withdrawal.
    #[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
    pub(crate) value: Option<(String, String)>,
}

/// The `value_type` and `value` columns of a withdrawal: empty, which no valued receipt has
/// (a receipt's type is a name). Since 0.6.0; both backends.
#[cfg_attr(not(feature = "sqlite"), allow(dead_code))]
pub(crate) const WITHDRAWN_VALUE: &str = "";

/// Parses receipt bytes as a valued [`Receipt`] or, failing that, a [`Withdrawal`] (steps 1-4
/// of either). The error is the receipt's when the bytes are neither.
pub(crate) fn parse_receipt(bytes: &[u8]) -> Result<Parsed> {
    // A receipt with a `withdrawn` member is a withdrawal: parsed by the core's `Withdrawal`
    // alone, so its own error (not the receipt's) is the one refused.
    let withdrawal = datumesh_core::json::parse(bytes).is_ok_and(|v| v.get("withdrawn").is_some());
    if withdrawal {
        let w = datumesh_core::Withdrawal::from_json(bytes)?;
        return Ok(Parsed {
            canonical: datumesh_core::json::parse(bytes)?.to_jcs(),
            hash: w.hash().to_string(),
            cell_id: w.cell().id(),
            tx: w.tx().to_string(),
            asof: w.asof().to_string(),
            fn_hash: w.cell().fn_hash.clone(),
            publisher: w.publisher().to_string(),
            cell: w.cell().clone(),
            host: w.signer_host().to_string(),
            entries: Vec::new(),
            inputs: Vec::new(),
            value: None,
        });
    }
    match datumesh_core::Receipt::from_json(bytes) {
        Ok(r) => {
            let (entries, inputs): (Vec<String>, Vec<String>) =
                r.inputs().iter().cloned().partition(|e| e.starts_with("slice:"));
            let value = r.value();
            Ok(Parsed {
                canonical: datumesh_core::json::parse(bytes)?.to_jcs(),
                hash: r.hash().to_string(),
                cell_id: r.cell().id(),
                tx: r.tx().to_string(),
                asof: r.asof().to_string(),
                fn_hash: r.cell().fn_hash.clone(),
                publisher: r.publisher().to_string(),
                cell: r.cell().clone(),
                host: r.signer_host().to_string(),
                entries,
                inputs,
                value: Some((value.ty().as_str().to_string(), value.lexical().to_string())),
            })
        }
        Err(e) => {
            let Ok(w) = datumesh_core::Withdrawal::from_json(bytes) else { return Err(e.into()) };
            Ok(Parsed {
                canonical: datumesh_core::json::parse(bytes)?.to_jcs(),
                hash: w.hash().to_string(),
                cell_id: w.cell().id(),
                tx: w.tx().to_string(),
                asof: w.asof().to_string(),
                fn_hash: w.cell().fn_hash.clone(),
                publisher: w.publisher().to_string(),
                cell: w.cell().clone(),
                host: w.signer_host().to_string(),
                entries: Vec::new(),
                inputs: Vec::new(),
                value: None,
            })
        }
    }
}

/// The value of stored receipt bytes: `Some` for a valued receipt, `None` for a withdrawal.
/// An error when the bytes are neither.
pub(crate) fn stored_value(bytes: &[u8]) -> std::result::Result<Option<Scalar>, datumesh_core::Error> {
    match datumesh_core::Receipt::from_json(bytes) {
        Ok(r) => Ok(Some(r.value().clone())),
        Err(e) => match datumesh_core::Withdrawal::from_json(bytes) {
            Ok(_) => Ok(None),
            Err(_) => Err(e),
        },
    }
}

/// The error of closing a name with no binding to close (never bound, or closed already).
pub(crate) fn unbound(name: &str) -> Error {
    Error::NotFound { what: "binding", key: name.to_string() }
}

/// `Ok` when `at` is later than the latest binding `latest` of `key` (a closing's tx).
pub(crate) fn later_than(what: &'static str, key: &str, at: &str, latest: &str) -> Result<()> {
    if at <= latest {
        return Err(Error::NotLater { what, key: key.to_string(), given: at.to_string(), latest: latest.to_string() });
    }
    Ok(())
}

/// A version and its receipt's bytes, for the default `*_with_receipt` methods.
fn with_receipt<S: Store + ?Sized>(store: &S, v: Version) -> Result<(Version, Cow<'_, [u8]>)> {
    match store.receipt(&v.receipt_hash) {
        Some(bytes) => Ok((v, bytes)),
        None => Err(Error::NotFound { what: "receipt", key: v.receipt_hash }),
    }
}
