# Changelog

All notable changes to `datumesh-store` and `datumesh-store-cli`.

## 0.5.3

Additive: catalogue facets and `extends` (CONTRACT-classes-round C2 and K9; spec 16.8 and 9.6). Four methods on `Store` (`upsert_catalog_ext`, `catalog_entry_ext`, `catalog_entries_with_facets`, `extensions_of`), four types (`CatalogEntryExt`, `FacetedEntry`, `Facets`, and the `CatalogEntryExt::new` constructor); nothing is removed and no existing method or `CatalogEntry` field changes. `upsert_catalog` leaves the facets and `extends` of an existing row as they are. Migrations 12 (PostgreSQL) and 8 (SQLite) add the columns `facets` (JSON text, `'{}'` by default, a JSON object) and `extends` (NULL by default) to `catalog_entries`; no stored value changes. cargo-semver-checks against 0.5.2 with a patch release type: no semver update required (229 checks pass, 31 skipped), so this is 0.5.3. `datumesh-store-cli` 0.5.3 follows the store version and pins it exactly.

### Added

- `Store::upsert_catalog_ext(&[CatalogEntryExt])`: inserts or replaces entries with their facets and `extends`, all or nothing. The store refuses a facet key other than `domain`, `thing`, `measure`, `region` or `frequency`, a facet of 0 or more than 8 names, a name that is not a name (2.2) or is longer than 32 bytes, names not ascending and unique, and an `extends` that is not a full name or is on an entry that is not a function (`Error::Invalid`).
- `Store::catalog_entry_ext(kind, name)`: an entry with its facets and `extends`.
- `Store::catalog_entries_with_facets()`: the entries that have a facet, by kind then name, each with its first binding time (`FacetedEntry::first_bound`, `None` when never bound).
- `Store::extensions_of(class)`: the functions whose `extends` is `class`, by name.
- Each new method has a default in the trait (`Error::Unsupported`), so an `impl Store` that delegates only the 0.5.2 methods still compiles.

The store does not check that `extends` names a stored class, nor the key rule of 16.3: http does that when it publishes.

## 0.5.2 (unreleased)

Additive: the function names of tests, bounded reads, exact dictionary and catalogue lookups (CONTRACT-0.4.9, Store 0.5.2, S1-S8). Fourteen methods, one type (`TestSummary`); nothing is removed and no existing method changes. Migrations 11 (PostgreSQL) and 7 (SQLite) add a column, an index and a table; no stored value changes, and a store of 0.5.1 keeps its function tests unnamed.

### Added

- `function_tests.fn_name` (NULL until named) with an index on `(fn_hash, fn_name, case)`, and table `open_keys (owner, key, marked_at)`.
- `Store::name_function_tests(fn_hash, names)`: names the tests of the given cases; a name once set is never changed. Returns the rows changed.
- `Store::unnamed_function_tests(limit)`: up to `limit` unnamed tests, by (fn_hash, case).
- `Store::function_test_summary(fn_hash, fn_name)`: the counts (`pass`, `fail`, `error`, `not_run`), the latest run and the branch counts, in one aggregate; `fn_name` `None` is every test of the hash. `TestSummary` (non-exhaustive, `Default`) is exported at the root.
- `Store::function_tests_page(fn_hash, fn_name, after_case, limit)`: a page of tests by case, with a SQL `LIMIT`.
- `Store::stored_receipts_of(ns, fn_name, fn_hash, limit)`: the receipts of the latest stored version of up to `limit` cells of a function, lowest cell id first.
- `Store::slice_values_limit(slice, asof, limit)` and `Store::versions_limit(cell_id, from, to, limit)`: the first `limit` members of `slice_values` and `versions`, in the same order, with the `LIMIT` in SQL.
- `Store::stored_cell_count(ns, fn_name, fn_hash)`: the cells of a function name and hash that have a stored version.
- `Store::has_key_value(owner, key, value)`, `Store::key_value_count(owner, key)`: exact dictionary lookups.
- `Store::open_key_count()`: the number of keys marked open, of any owner.
- `Store::mark_key_open(owner, key)` and `Store::key_is_open(owner, key)`: an open key takes no automatic values (rule H7 of the mesh).
- `Store::catalog_entry(name)`: the catalogue entry of an exact name (a function's before a method's before a class's).

Every new method has a default in the trait, so an `impl Store` that delegates only the 0.5.1 methods still compiles. The defaults answer from the 0.5.1 methods where they can (`slice_values_limit`, `versions_limit`, `function_test_summary` and `function_tests_page` without a name, and `has_key_value` through `text_find`), and are `Error::Unsupported` otherwise.

## 0.5.1

Additive, and one fix: function tests, the index of descriptor inputs and the names bound to a function (Build 2, Part 3, calculus). Six methods and one type; nothing is removed. `public-api.txt` gains them (`tools/public_api.sh`). Migrations 10 (PostgreSQL) and 6 (SQLite) add a table, two indexes and the descriptor-inputs table; the migrations also back-fill the inputs index from the descriptors already stored. No existing table or stored value changes.

### Added

- Table `function_tests`: one row per (function hash, case), with the canonical test line, the signer, the last run (instant, result, detail) and the branch counts of the function. Append-friendly: no trigger.
- Table `descriptor_inputs`: one row per fully qualified name in a descriptor's `inputs`, with an index on the name. `put_descriptor` writes it in the descriptor's transaction (both backends; a descriptor with no inputs indexes nothing). The migrations back-fill it from the descriptors stored before 0.5.1, from their JSON. An index on `name_bindings (fn_hash, fn_name)` serves `bound_names`.
- `FunctionTest` (at the root, non-exhaustive) with `FunctionTest::new(fn_hash, case, test_json, signed_by)`.
- `Store::put_function_test(test)`: inserts a test, once per (fn_hash, case). `Ok(true)` when inserted; `Ok(false)` and no change when the row exists (the first writer's signer and test line stay).
- `Store::function_tests(fn_hash)`: the tests of a function, ordered by case.
- `Store::record_function_test(fn_hash, case, run_at, result, detail)`: sets the last run of an existing test. `Ok(false)` when there is no such test. `result` is `pass`, `fail` or `error`; any other value is `Error::Invalid`.
- `Store::set_function_branches(fn_hash, total, covered)`: sets the branch counts on every test of the function (a no-op when there are none). `covered` above `total` is `Error::Invalid`.
- `Store::function_readers(name)`: the function hashes of the stored descriptors whose `inputs` contain `name`, distinct, ascending.
- `Store::bound_names(fn_hash)`: every name ever bound to the hash, distinct, ascending. A name since bound to another hash still lists this one.

### Fixed

- Dropping a `PgStore` on a thread that is inside a tokio runtime (an async task, a `block_on`) panicked: the sync client runs a runtime of its own, which cannot be dropped there. The pool now closes its connections on a detached thread, which nothing joins; a connection the pool discards is closed the same way.

## 0.5.0

Additive: the catalogue (Build 2, Part 2, R1). Ten methods, and the types they take and return; nothing is removed. `public-api.txt` gains them (`tools/public_api.sh`). Migrations 9 (PostgreSQL) and 5 (SQLite) add tables; no existing table or stored value changes.

### Added

- Tables (`CONTRACT-build2-part2-read` R1). `catalog_entries`: one row per (kind, name), with label, description, arguments, examples, source (publisher or operator) and signer. `key_values`: the dictionary of a key of an owner, with a label, aliases (the publisher's) and `n_cells`. `find_misses`: query text and time, no other column. On PostgreSQL, `embeddings` (model, ref kind, ref id, `vector(384)`, an HNSW index on cosine distance), made only where the `vector` extension is available (`pg_available_extensions`), with `CREATE EXTENSION IF NOT EXISTS vector`. Trigram indexes (`pg_trgm`) where that extension is available. SQLite has the same tables without vectors.
- `Store::upsert_catalog(entries)`: insert or replace by (kind, name), all or nothing, with the bounds checked first (names by the core's rules, a label of at most 120 bytes, a description of 1 to 1024 bytes). An operator's entry never replaces a publisher's. Returns the ids.
- `Store::upsert_key_values(owner, key, values)`: adds and replaces values (label and aliases); does not prune, and keeps `n_cells`. Returns the ids.
- `Store::refresh_value_counts()`: sets each value's `n_cells` to the number of cells of the owner's function with that value for the key.
- `Store::text_find(query, limit)`: entries and values matching every word of the query, case-insensitively and literally (`%`, `_` and `\` are characters). A word scores 1.0 as a whole field, 0.8 as a word of a field, 0.5 inside a field; the score is the mean over the words, so it lies in (0, 1]. Best first, ties by name.
- `Store::vector_find(model, vec, limit)` and `Store::put_embeddings(model, items)`: cosine search over the embeddings of one model (score = 1 minus the cosine distance) and storage of them, on PostgreSQL with the `vector` extension. A vector has 384 finite values, not all zero. A reference to an entry or value that is not stored is refused. Changing an entry's text or a value's label drops its embedding.
- `Store::pending_embeddings(model, limit)`: the entries and values with no embedding for the model, with the text to embed.
- `Store::vector_available()`: whether the `embeddings` table exists, looked at when the store is opened.
- `Store::log_miss(query)`: records a search that found nothing (trimmed, at most 512 bytes on a character boundary; a blank query records nothing). The newest `FIND_MISSES_CAP` (100,000) rows are kept.
- `Store::catalog_summary()`: every entry with its label and cell count, and every dictionary with its number of values and its first 50 by value.
- `Error::Unsupported`: a vector call where `vector_available()` is false (SQLite, or PostgreSQL without the extension).
- Types at the root: `CatalogEntry`, `CatalogKind`, `CatalogSource`, `KeyValue`, `EmbedRef`, `Embedding`, `PendingEmbedding`, `Hit` (non-exhaustive), `Summary`, `SummaryEntry`, `Dictionary`, `DictionaryValue`, and `FIND_MISSES_CAP`.

## 0.4.3

Additive: three methods and one type; nothing is removed. `public-api.txt` gains them (`tools/public_api.sh`). No change to any receipt, hash or stored value: the same rows as calling `put_receipt` and `put_version` one by one.

### Added

- `Store::latest_many(cell_ids)`: `latest` of each id, one entry per input in input order (duplicates allowed; empty input, empty result). The first malformed id is the error, as `latest` gives it. PostgreSQL answers up to 10,000 ids per statement.
- `Store::receipts_many(hashes)`: `receipt` of each hash, in input order, as owned bytes (`None` for a missing or malformed hash). PostgreSQL: one statement per 10,000 hashes.
- `Store::put_cells(items)` with `CellWrite` (exported at the root): for each item in order, `put_receipt` of its receipt and, when it has a version, `put_version` of that cell, tx and the receipt's hash. Returns the receipt hashes in order, and every check and error is the one of those two calls, for the first item that fails.
  - PostgreSQL: one transaction, all or nothing. The round trips do not grow with the items in the normal case (the stored receipts of the call with one `= ANY`, the key documents of the signer hosts, the version locks in one statement in item order, the latest tx of the cells, then one multi-row insert each for receipts and versions; edges only for receipts with inputs). Signatures are verified on up to `available_parallelism()` threads. Calls of up to 10,000 items are the size it is written for.
  - SQLite: the default method, which loops over `put_receipt` and `put_version` and stops at the first error. It is not atomic: the items before a failing one stay stored, as they would after the same loop.

### Changed

- The slice statements (members, values, changed-since and computed cells) check each value-bound argument with an index-only probe of `cell_args_value`, and a driven slice's candidates are a materialized CTE joined to `cells` by primary key (PostgreSQL). The rows are the same; no schema change and no new index. SQLite is unchanged.
- PostgreSQL migration 8 (`0008_index_cell_per_statement.sql`): the cell index of a stored receipt runs once per statement, not once per receipt. It makes the same `cells` and `cell_args` rows; no backfill, no change to any stored value. SQLite is unchanged.

## 0.4.2

Additive: the trait is sealed, and the new methods are required, with no default. `public-api.txt` gains 8 additions and no removals (`tools/public_api.sh`).

### Added

- `Store::computed_cells(slice, asof, limit)`, on both backends: the cells matching a slice (as
  `slice_values` matches them: the fixed arguments, `fn_hash`, `ns`, `fn` and the number of
  arguments) that have a computed version answering as of `asof` (`tx <= asof` and the receipt's
  asof `<= asof`, as `computed_asof` answers). Ascending by cell id, at most `limit` (`slice_values`
  takes no limit, so this one does; a `limit` of 0 gives none), each `Cell` with its arguments from
  the typed-argument index. A cell with only a stored version is not returned.
- `Store::has_bindings_under(namespace)`: whether any name binding or class binding has a name
  starting with `namespace` and `/` (`"a.example/n"` is not a namespace of `a.example/ns/f`).
- Migrations: PostgreSQL 0007 (`binding_audit`) and SQLite 0004 (`binding_audit`): the audit
  column `written_at` on `name_bindings` and `class_bindings`. The database fills it at insert
  (PostgreSQL `DEFAULT now()`, SQLite the insert statement's `strftime('%Y-%m-%dT%H:%M:%fZ','now')`).
  Rows written before the migration are `NULL`. It is in no hash and read by no API. The
  append-only triggers are unchanged. Declarations are key-document bytes in `namespaces`, not
  rows, so they get no column.

## 0.4.1 (unreleased; branch step-1.5-namespace-validation, held for the spec-0.5.0 round)

Additive: cargo-semver-checks 0.51.0 against 0.4.0 finds no semver update required (SEMVER_RESULT). `datumesh-store-cli` 0.4.1 follows the store version; its changes are the export and import of classes below.

Classes (SPEC 16.1 to 16.3, 9.7): a class descriptor is stored by its class hash, and a class
name is bound over transaction time to a class, with its methods bound in the same transaction.
The store records what it is given. The owner rule (16.2 step 1) is the definer's check, not the
store's: http and the engine enforce it. Requires `datumesh-core` 0.3.8 (not yet published; the
lock is updated at release).

Domain validation (step 1.5): the store records the outcome of each check of a host's key
document, suspends a namespace after consecutive failures and refuses new writes under a
suspended one. Additions only (the trait is sealed; `Namespace` and `Error` are
non-exhaustive); new migrations, none edited.

### Added

- Migrations: PostgreSQL 0005 and SQLite 0002 (`namespace_checks`): `namespaces.consecutive_failures`
  (not null, default 0) and `namespaces.last_checked_at` (PostgreSQL also `last_checked_at_at`).
- `Namespace::consecutive_failures` and `Namespace::last_checked_at`.
- `Store::record_namespace_failure(host, checked_at, threshold)`: counts the failure and
  suspends (`suspended_at`, the single record of suspension) when the count reaches the
  threshold; `Store::record_namespace_success(host, checked_at)`: resets the count and
  reinstates; `Store::namespaces()`: every namespace, by host.
- `Error::NamespaceSuspended { host, suspended_at }`: new writes under a suspended namespace
  (`put_receipt` of a receipt signed by the host, `put_version` of such a receipt,
  `bind_name` of a name under the host) are refused with it. Reads, verification, putting a
  receipt that is already stored, and `put_namespace` (a rotation) are not affected.

- Migrations: PostgreSQL 0006 and SQLite 0003 (`classes`): `classes` (the canonical flattened
  class descriptors by class hash; PostgreSQL also checks the hash against the bytes) and
  `class_bindings` (a class name over tx), append-only and in tx order as the other tables. On
  PostgreSQL triggers keep the one namespace of names. Existing rows are untouched.
- `Store::put_class(bytes)`: stores a flattened class descriptor under its class hash and returns
  the hash. The core checks it (`ClassDescriptor::from_json`). The bytes must be the canonical
  form: other bytes are refused with `E_SCHEMA` and nothing is stored. Idempotent. Refused with
  `Error::NamespaceSuspended` while the steward's namespace is suspended, even for a class
  already stored. The store does not check the owner (16.2 step 1; see above).
- `Store::class(class_hash)`: the canonical bytes of a stored class, `None` when not stored.
  Never refused under a suspended namespace.
- `Store::bind_class(class_name, class_hash, tx)`: binds the class name, and each method
  `<class name>/<m>` of the flattened descriptor to its function hash, in one transaction. The
  class and every method descriptor must be stored (`NotFound`, `what` `"class"` or
  `"descriptor"`); `tx` must be later than the latest binding of each name (`NotLater`). On any
  failure nothing is bound. Refused with `NamespaceSuspended` while the host of the class name is
  suspended.
- `Store::resolve_class(class_name, asof)` and `Store::resolve_class_binding(class_name, asof)`
  (with `ClassBinding { class_hash, tx }`, as `resolve_binding` gives `Binding`): the class
  name's binding as of `asof`.
- `Error::NameTaken { name, bound_as }`: class names and function names are one namespace.
  `bind_class` of a name bound as a function, and `bind_name` of a name bound as a class, are
  refused with it; so is a method name bound as a class. A method name bound as a function is
  bound again.
- `datumesh-store-cli`: `export` writes the classes (`{"class":…}`, after the descriptors) and
  the class bindings (`{"class_binding":…}`); `import` reads them back. A store with no class
  bindings exports exactly what 0.4.0 did; with them, the names and class bindings are in one
  tx-ordered list. No new commands.

## 0.4.0 (2026-11-02)

For the local resolver (step 1.8a): a second backend, SQLite, in one file with no server,
implementing the sealed `Store` with the same behaviour as `PgStore`. Additions only for
users of `PgStore` (`API-CHANGES-0.4.0.md`); 0.4.0 because the trait gains a backend.

### Added

- `SqliteStore` (`open`, `open_with`, `connect` for `sqlite:` URLs, `path`,
  `check_statements`) and `SqliteOptions` (`max_size`, `busy_timeout`, `durable`): every
  `Store` and `Resolver` method, every error code and rule of `PgStore`. SQLite is compiled
  in (`rusqlite` 0.40 with `bundled`), behind the feature `sqlite`, on by default
  (`default-features = false` builds the PostgreSQL store alone).
- `SqliteStore::open_existing` (no file is created; a missing path is `no such database
  file`) and `migrate::sqlite::check_file(path)`; `open` creates a missing file. The
  command's `migrate --check` on SQLite creates nothing.
- `migrate::sqlite::{MIGRATIONS, apply, catalog, check}` and `migrations/sqlite/0001_initial.sql`:
  the schema of PostgreSQL migrations 1-4 in one, with the differences listed in the README.
- Concurrency on one file from several threads and processes: WAL, a busy timeout,
  `BEGIN IMMEDIATE` for every write, triggers for the append-only and order rules (on any
  connection), a bounded write-ahead log.
- Exact decimals in SQLite's typed-argument index: a sortable text encoding (`sort_key`).
- `DatabaseError::code` gives SQLite's result code name (`SQLITE_BUSY`, ...) for SQLite.
- The command takes a SQLite file as `DATABASE_URL` (a path or a `sqlite:` URL): `migrate`
  (`--check` against a scratch database in memory), `import`, `export` (the same lines as
  PostgreSQL's for the same contents), `bench` (the same rows).
- Tests: one conformance suite runs every shared test against both backends (`pg::*`,
  `sqlite::*`); `tests/sqlite_processes.rs` (several processes and threads on one file).

### Changed

- `DatabaseError`'s source is the driver's error behind a `Box<dyn Error + Send + Sync>`
  (private; `source()` still returns it).
- `datumesh-store-cli` 0.4.0 depends on `datumesh-store = "=0.4.0"` and on `rusqlite`.

## 0.3.2 (2026-10-26)

For the engine 0.3.1: the members of a slice as values, and a cheap check that a slice does
not change after A. Additions only (`API-CHANGES-0.3.2.md`).

### Added

- `Store::slice_values(slice, asof) -> Result<Vec<SliceValue>>`: the members
  `slice_members` gives, in its order and by its answering rule, each as its version, its
  receipt's typed value and its cell (with its arguments), without receipt bytes.
- `SliceValue { version, value, cell }` (`#[non_exhaustive]`).
- `Store::slice_changed_after(slice, asof) -> Result<bool>`: whether a cell matching the
  slice (for a version slice, its cell with a `tx` in `[from, to]`) has a stored version whose
  receipt's asof is after A, with index scans only: two bounded probes choose between the
  function's receipts with an asof after A (new index `receipts (fn_hash, asof)`) and the
  slice's candidate cells (new index `receipts (cell_id, asof)`), and one query walks the
  smaller, stopping at the first match. `false` means the members and their versions answer the same as of
  every later time until such a version is stored; `true` can be conservative.
- Migration 4 (`0004_receipt_values.sql`): `receipts.value_type` and `receipts.value`
  (backfilled from the canonical bytes, checked equal to them, filled by a trigger when an
  insert does not name them, as `asof` is), and the indexes `receipts_fn_hash_asof`, `receipts_cell_asof`.
- The command: `bench` times `slice_values` and `slice_changed_after` on the shapes of
  `slice_members`.

### Changed

- `datumesh-store-cli` 0.3.2 depends on `datumesh-store = "=0.3.2"`.

## 0.3.1 (2026-10-25)

For the formula dialect (spec-0.4.0, slice-as-lineage): the members of a slice as of A,
version ranges, manifests, and receipts whose inputs cite slices. Additions only
(`API-CHANGES-0.3.1.md`); `Store` is sealed, so its new required methods break no
implementor.

### Added

- `Store::slice_members(slice, asof) -> Result<Vec<SliceMember>>`: the members of a
  `datumesh_core::Slice` as of A (SPEC 15.13): the cells of its function whose bound
  arguments are equal, ranged ones within the range in the type's order (numbers as exact
  decimals, dates and instants chronologically, strings by code point) and open ones any
  value, with the stored version answering as of A (`tx <= A` and the receipt's
  `asof <= A`, as `stored_asof`) and its receipt's bytes; in key order (open and ranged
  arguments by ascending name, each in its type's order). A version slice is
  `versions_between` of its cell. Found with a range scan of a new typed-argument index.
- `Store::versions_between(cell_id, from, to, asof)`: the stored versions with
  `from <= tx <= to` answering as of A, ascending, with their receipts (a version slice).
- `SliceMember { version, receipt }` (`#[non_exhaustive]`).
- `Store::put_manifest(bytes)`: a manifest, checked by the core (`Manifest::from_json`),
  stored canonical under its entry (returned); idempotent, insert-only; members need not
  be stored. `Resolver::manifest(entry)` of `PgStore` serves it (core 0.3.4's provided
  method, overridden). `Store::member_manifests(receipt_hash)`: the manifests listing a
  receipt.
- `Store::slice_entries(receipt_hash)`, `Store::slice_dependents(entry)`: the slice-entry
  lineage, both ways.
- Migration 3 (`0003_slices.sql`): `cells` and `cell_args` (the typed-argument index,
  filled from the canonical receipt bytes by a trigger on `receipts` and backfilled for
  every stored receipt), `manifests`, `manifest_members`, `slice_edges`; all insert-only.
- The command: `export`/`import` carry manifests (`{"manifest": "<JSON text>"}` lines);
  `import` takes the manifests of vectors; `bench` times the new methods
  (`--slice-cells N`, default 100,000).

### Changed

- `put_receipt` accepts receipts whose `inputs` hold slice entries (core 0.3.4 verifies them
  at level signed) and records each entry; its manifest is not required, before or after
  (SPEC 10.1 needs it only to recompute). `inputs` and `dependents` keep listing receipt
  hashes only. `cell_dependents` also finds the cells reading a cell through a slice whose
  stored manifest lists one of its receipts. (No receipt stored by 0.3.0 has a slice entry,
  so nothing changes for existing data.)
- `put_descriptor` (and `evaluate`, reading a stored descriptor) checks descriptors as
  spec-0.4.0's `FunctionDescriptor`: `signature.order` and argument `extent` are accepted
  (0.3.0 refused them with `E_SCHEMA`, as a spec-0.3.0 verifier must); formula
  descriptors always carry `order`. A descriptor without them is checked as before.
- Requires `datumesh-core` 0.3.4 (was 0.3.3). The `spec` submodule moves to `spec-0.4.0`
  (f3d3e2c).
- `datumesh-store-cli` 0.3.1 depends on `datumesh-store = "=0.3.1"`.

## 0.3.0 (2026-10-24)

For the engine: when a name binding began. `Store` is sealed, and the `Resolver`'s
`evaluate` answers with both times. Breaking (`API-CHANGES-0.3.0.md`): a new required
`Store` method, and the seal, for code outside this crate that implemented `Store`; the
behaviour of `evaluate` for late-signed receipts.

### Added

- `Store::resolve_binding(fn_name, asof) -> Result<Option<Binding>>`: the name binding that
  answers as of A, the one with the latest `tx <= A`, with its function hash and its `tx`
  (when it began). A name bound again to a hash it meant before, even the hash of the
  binding before it, begins again at the later `tx`. Malformed arguments are the core's
  codes, without a query. `resolve_name` is unchanged (it is the binding's `fn_hash`).
- `Binding { fn_hash, tx }` (`#[non_exhaustive]`).
- The command: `bench` times `resolve_binding` and `resolve_name` (100 names with 100
  bindings each).

### Changed

- **`Store` is sealed** (a private supertrait): only this crate implements it. The store is
  the trust boundary (it verifies every receipt against its host's key document before it
  keeps it, and callers rely on what it answers as checked). Code that implemented `Store`
  (a wrapper, an in-memory store for tests) no longer compiles. `Store` is still object-safe
  (`Arc<dyn Store + Send + Sync>`).
- **`Resolver::evaluate` of `PgStore` answers with both times**: the value of a stored
  function as of A is that of `stored_asof`'s version (the latest by tx with `tx <= A` and
  its receipt's `asof <= A`), not `version_asof`'s (tx only). A late-signed receipt (asof
  later than tx) gives a value only from its asof on; until then the version before it, or
  none. Nothing changes when publishers set `asof = tx` (spec-0.3.0's SHOULD). This is
  `datumesh-core` 0.3.3's testkit `AnswerRule::TxAndAsof`, its default, which the
  conformance test runs. `version_asof` keeps its tx-only meaning.
- Requires `datumesh-core` 0.3.3 (was 0.3.2). The `spec` submodule stays at `spec-0.3.0`.
- `datumesh-store-cli` 0.3.0 depends on `datumesh-store = "=0.3.0"`.

## 0.2.0 (2026-10-24)

Additions for the engine: computed versions, answering as of A with both times, cells that
read a cell. No existing method, type or behaviour changes; adding methods to the `Store`
trait is breaking for other implementors of it, hence 0.2.0 (`API-CHANGES-0.2.0.md`).

### Added

- `Store::put_computed(cell_id, tx, receipt_hash)`: a computed version of a cell. Computed
  versions are derived, not facts: insert-only, added in any order (historical backfill),
  keyed on (cell_id, tx, asof) with the receipt's asof (two computed receipts of one cell
  may share a tx and differ in asof). Idempotent; another receipt at an existing key is
  `Error::Conflict` (`hash`: the receipt refused); the receipt must be stored, of the cell,
  at the tx (`NotFound`, `Mismatch`, as for `put_version`). Concurrent writers of one key:
  one wins, the others get `Conflict`.
- `Store::computed_asof(cell_id, asof)`: the greatest asof <= A (ties: the greatest tx).
- `Store::stored_asof(cell_id, asof)`: the latest stored version (by tx) with tx <= A whose
  receipt's asof is also <= A. A late-signed receipt answers from its asof on.
  `version_asof` keeps its tx-only meaning.
- `Store::computed_asof_with_receipt` / `stored_asof_with_receipt`: the answer and its
  receipt's bytes; provided methods, one query in `PgStore` (about 45 % faster than two
  calls).
- `Store::cell_dependents(cell_id)`: the cells of the stored receipts that list any stored
  receipt of the cell among their inputs; ascending, unique.
- `Version::asof`: the receipt's asof, for every method returning a `Version`.
- Migration 2 (`0002_computed_versions.sql`, additive): `receipts.asof` / `asof_at`,
  backfilled from the stored canonical bytes, checked equal to them, and filled from them
  when an insert does not name the column (a 0.1.x writer); the table `computed_versions`
  (insert-only like `versions`, without the tx-order rule). Apply it with
  `datumesh-store migrate`; `migrate --check` knows it.
- The command: `export` writes computed versions (`{"computed":{"asof","cell_id",
  "receipt_hash","tx"}}` lines, after the versions), `import` reads them and still reads
  0.1.x exports; `bench` times the new reads and writes.

## 0.1.2 (2026-10-24)

- Requires `datumesh-core` 0.3.2 (was 0.3.1), which implements specification `spec-0.3.0`:
  an input receipt's `asof` may be at or before the receipt's, an `ok` envelope's receipt
  `asof` at or before the requested one, and `E_EVAL_UNAVAILABLE` when a function cannot be
  evaluated. The tests run the `spec-0.3.0` vectors. No API change.
- The first release published with crates.io Trusted Publishing.

## 0.1.1 (2026-10-22)

No type of the `postgres` driver or of a TLS crate is part of the API any more, except in
the new module `pg_unstable`, which is outside semver until 1.0. TLS through `PoolOptions`.
Published with an API token: Trusted Publishing was refused on the `v0.1.1` run (see 0.1.2).

### Changed (breaking for code that named `postgres` types; 0.1.0 had 16 downloads)

- `pub use postgres` at the crate root: removed. The driver is re-exported as
  `pg_unstable::postgres`, outside semver.
- `migrate::apply(&mut postgres::Client)`, `migrate::catalog(&mut postgres::Client)`,
  `migrate::check(&mut Client, &mut Client)`: they take the store instead,
  `migrate::apply(&PgStore)`, `migrate::catalog(&PgStore)`,
  `migrate::check(live: &PgStore, scratch: &PgStore)`. `apply` holds one pooled connection
  throughout (the advisory lock and the tracking table on one session). A `PgStore` connects
  to an empty database without touching it, so `migrate::apply(&PgStore::connect(url)?)`
  makes the schema.
- `Error::Database(postgres::Error)`: now `Error::Database(DatabaseError)`, an opaque struct
  with `message()` (the server's message, or the driver's text with its causes), `code()`
  (the SQLSTATE, e.g. `42P01`) and the driver's error as `source()` (type erased). Display
  is as before (`database: <message> (<SQLSTATE>)`). For a failure that is not the
  server's, the message now includes the driver's causes (`error performing TLS handshake:
  invalid peer certificate: UnknownIssuer` rather than `error performing TLS handshake`).
- `impl From<postgres::Error> for Error`: removed (the conversion is internal).
- `PgStore::with_connector(f, PoolOptions)`: moved to `pg_unstable::with_connector(f,
  PoolOptions)`, same signature, outside semver. `PgStore::connect_with` covers TLS.
- `PgStore::connect(url)` now uses TLS when the server offers it (the default
  `sslmode=prefer`), verifying the certificate: a server with `ssl=on` and a certificate
  the Mozilla roots do not cover now fails to connect; add its CA, or use
  `sslmode=disable`. In 0.1.0 it never used TLS.

### Added

- TLS: `PgStore::connect_with(url, PoolOptions)`; `connect` and `connect_with` use rustls
  (`tokio-postgres-rustls` 0.14, `rustls` 0.23 with the ring provider chosen per connector,
  no process-wide provider) as the URL's `sslmode` says (`disable`, `prefer`, `require`),
  verifying the server's certificate and host name against the Mozilla roots
  (`webpki-roots`) plus `PoolOptions::tls_root_certificates_pem(pem)`;
  `PoolOptions::tls_webpki_roots(false)` trusts only the added roots. No option accepts any
  certificate.
- `Error::Config(String)`: a root certificate bundle with no certificate, or one that does
  not parse.
- `DatabaseError` (see above).
- `pg_unstable` (outside semver until 1.0): `with_connector`, `Connector` (plain
  `postgres` clients that connect as the store does), `pub use postgres`.
- The command: TLS for every subcommand (`DATABASE_URL`'s `sslmode`), `DATABASE_CA_FILE`
  for roots to trust besides the Mozilla roots; connection errors show their causes. It
  depends on `datumesh-store = "=0.1.1"` exactly (it uses `pg_unstable`).

## 0.1.0 (2026-10-22)

First release, for `datumesh-core` 0.3.1 (specification `spec-0.2.1`).

- `Store` trait (a `datumesh_core::Resolver` that can be written to) and `PgStore` on
  PostgreSQL 16: descriptors checked by the core and stored canonical; receipts verified
  (SPEC 10.1 steps 1-6) against the key document stored for their signer host and stored
  canonical, so nothing unverified enters the store (putting a stored receipt again is
  idempotent and not re-checked, even after its key is revoked); blobs;
  append-only versions over transaction time and name bindings, enforced by triggers and
  serialised per cell / per name; lineage edges; namespaces' key documents. Passes the
  core's resolver test kit as a time-aware resolver.
- Synchronous API (`postgres` driver) with the crate's own small connection pool and
  per-connection prepared statements; typed `Error`. Every `Store` method, reads included,
  returns a `Result` (malformed arguments are `Error::Invalid`, database failures
  `Error::Database`); the `Resolver` methods keep `Option` and log failures with `tracing`.
- `migrate`: embedded SQL migrations with a tracking table, a catalog dump and a schema
  check against a scratch database.
- `datumesh-store` command: `migrate [--check]`, `import`, `export`, `bench`.
