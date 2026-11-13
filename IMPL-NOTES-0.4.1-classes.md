# Implementation notes: datumesh-store 0.4.1, classes

For the implementer of `CONTRACT-store-0.4.1-classes.md`. The tests commit holds the API as
stubs (`todo!()` in `PgStore` and `SqliteStore`), the docs of the new `Store` methods (they are
the behaviour spec; refine them, keep the rules), and the failing tests. Spec: `spec-0.5.0`
(`/root/datumesh-spec`, commit `6d5633e`), 9.7, 16.1 to 16.3. Core: `datumesh-core` 0.3.8
(`ClassDescriptor`, `define_class`, `ClassEnv`), by a local, uncommitted `[patch.crates-io]`.

The tests commit changes the workspace `datumesh-core` requirement to `"0.3.8"` (the tests
use the class API). It does not touch `Cargo.lock`: it has no 0.3.8 to lock until core is
published; the lock is updated at release time.

## API (all additive; `Store` is sealed)

On `Store` (and so on `PgStore` and `SqliteStore`):

| Method | Meaning |
|---|---|
| `put_class(&self, &[u8]) -> Result<String>` | stores a flattened descriptor, canonical bytes, by class hash |
| `class(&self, &str) -> Result<Option<Vec<u8>>>` | the stored canonical bytes |
| `bind_class(&self, class_name, class_hash, tx) -> Result<()>` | binds the name and every method `N/m`, one transaction |
| `resolve_class(&self, class_name, asof) -> Result<Option<String>>` | class hash as of `asof` |
| `resolve_class_binding(&self, class_name, asof) -> Result<Option<ClassBinding>>` | the same with `tx` (the `since` of 9.7) |

New types: `ClassBinding { class_hash, tx }` (`#[non_exhaustive]`, like `Binding`) and
`Error::NameTaken { name: String, bound_as: &'static str }` (`"function"` or `"class"`).
`Error::NotFound.what` gains the value `"class"`. `resolve_class_binding` is not in the
contract's list; it is there because the bindings answer of 9.7 needs `since` (as
`resolve_binding` gives it for functions). Drop it if you disagree, with its tests.

The `Store` doc example that `tests/sealed.rs` compiles lists every method; it already has the
five new ones.

## The rules the tests pin

1. **`put_class`.** `ClassDescriptor::from_json(bytes)` (core's check; its error is returned
   as `Error::Invalid`), then the bytes must equal `descriptor.canonical()`, otherwise
   `Error::Invalid` with code `E_SCHEMA` and nothing stored (core has no "not canonical"
   code; `E_SCHEMA` is my choice, see "Open points"). The class hash is
   `descriptor.hash()`, equal to the SHA-256 of the stored bytes. Idempotent. The store does
   not flatten and does not need the parent, methods or interfaces to be stored. No owner
   check (item 4: say so in the docs; the store records, http and the engine enforce).
2. **`class(hash)`.** `E_CANON_HEX` for a malformed hash (lowercase 64 hex, as everywhere);
   `None` when not stored. A class hash and a function hash are different rows: `class(fn_hash)`
   is `None`, `descriptor(class_hash)` is `None`.
3. **`bind_class`.** Argument checks first, without a query (`E_CANON_NAME`, `E_CANON_HEX`,
   `E_CANON_INSTANT`). Then, in one transaction (PostgreSQL: the append-only triggers and
   advisory locks of `name_bindings` apply to each row; SQLite: one write transaction):
   suspension of the host of `class_name` (as `bind_name`); the class stored
   (`NotFound { "class" }`); each method's descriptor stored, in JCS order of method names
   (`NotFound { "descriptor", <fn_hash> }`); the kind of `class_name` and of every method name
   (`NameTaken`); `tx` later than the latest binding of `class_name` and of each method name
   (`NotLater { what: "name", key: <that name> }`). Any failure binds nothing. Then insert the
   class binding and one function binding per method of the flattened descriptor (inherited
   methods included: the descriptor is flattened, so just its `methods`), all at `tx`.
4. **One namespace of names.** The latest binding of a name has a kind. `bind_name` of a name
   whose latest binding is a class is refused (`NameTaken { bound_as: "class" }`);
   `bind_class` of a name whose latest binding is a function is refused (`bound_as:
   "function"`), and so is a method name `N/m` whose latest binding is a class (it is a
   class of its own). A method name whose latest binding is a function is just bound again
   (to whatever hash; same `tx` rule). "Latest" means latest by `tx`, not "as of now".
5. **Accessors are kind-specific.** `resolve_name` and `resolve_binding` answer `None` for a
   class name; `resolve_class` and `resolve_class_binding` answer `None` for a function name
   (a method name is a function name). A bindings answer has `fn_hash` or `class_hash`, never
   both. Binding a class name again to the same hash begins again at the later `tx`.
6. **Suspension (step 1.5).** `put_class`: the host is the **steward's namespace**
   (`steward.namespace`), since `put_class` has no name; refused with
   `Error::NamespaceSuspended` even for a class already stored (a deliberate strictness; one
   test, `putting_a_stored_class_again_under_a_suspended_namespace_is_refused`, pins it and is
   easy to flip). `bind_class`: the host of the class name (the methods are under it too).
   Reads (`class`, `resolve_class*`, `resolve_name` of methods) are never refused. No key
   document is needed for `put_class` or `bind_class`.
7. **Migration.** PostgreSQL migration 6 and SQLite migration 3, exactly one on each backend
   (`the_class_migration_is_additive_and_keeps_function_bindings` checks `MIGRATIONS.len()`
   is 6 and 3, the old five and two untouched, and `apply` from the old state returns only the
   new one). The schema is yours: a class table (hash, canonical bytes, insert-only), and a
   way to tell a binding's kind (a kind column on `name_bindings` defaulting to `function`, or
   a class-bindings table with a trigger that keeps the one-namespace rule). Existing rows
   must read as function bindings. Keep the append-only and tx-order guarantees for the
   bindings of both kinds, and the checksums of migrations 1 to 5. Add the usual constraints
   (hash format, canonical bytes are JSON). `migrate --check` and `check_statements` must stay
   clean, so every new statement must be listed where the others are.

## Things to update that the tests commit leaves alone

- `crates/datumesh-store/tests/migration_0004.rs` asserts `migrate::apply(&s) == [4, 5]` on a
  0.3.1 database; with migration 6 it becomes `[4, 5, 6]`. `migration_0002.rs` and
  `migration_0003.rs` compare with a `later` list; check them too. These are old tests that
  stay green only until the migration lands; update their expected lists (that is the only
  change to old tests that should be needed).
- `public-api.txt` (regenerate with `tools/public_api.sh --write`); the new test
  `the_api_has_classes_since_0_4_1` reads it.
- `README.md`, `CHANGELOG.md` (the 0.4.1 entry covers the namespace checks and classes),
  `API-CHANGES-0.4.1.md` if you keep that habit.
- Docs of the owner rule (item 4) in the crate docs of `lib.rs`.
- `cargo semver-checks` against 0.4.0 for the implementation commit message.

## store-cli (item 5)

The CLI has no function commands (no put-descriptor, no bind-name): its commands are
`migrate`, `import`, `export`, `bench`. So there are **no new subcommands**; the contract asks
for them only if matching function commands exist. What the CLI does have, and must not make
lossy, is `export` and `import`: an export that omitted classes and class bindings would be a
silent loss in a backup. The test `export_and_import_keep_classes_and_class_bindings` (in
`crates/datumesh-store-cli/tests/cli_sqlite.rs`, module `classes`) requires that a store with
two class versions, their method bindings and a function name goes through export and import
unchanged: SQLite to SQLite (and again, as a no-op), PostgreSQL to SQLite and back, with the
same lines from both backends. The line formats are free (suggestion: `{"class":"<descriptor
JSON text>"}` after the descriptors, and a `name` line with `class_hash` in place of
`fn_hash`). Two things to watch:

- Importing a class binding binds the methods; the exported `N/m` name lines at the same `tx`
  then come as `NotLater` with the same hash, which `import` already treats as "imported
  before" for names (see its `"name"` arm). If you export the method rows too, that is enough;
  if you leave them out of the export, nothing more is needed. Either way, imported bindings
  must equal the exported ones.
- Existing export fixtures (`tests/fixtures/export-0.1.2.jsonl`, `export-0.3.0.jsonl`) must
  still export the same lines: a store with no classes exports exactly what 0.4.0 did.

If you would rather not extend export and import in 0.4.1, delete that test and report it as
a deviation; nothing else depends on it.

## Open points (decided in the tests, easy to change)

- Non-canonical bytes are `Error::Invalid` with `E_SCHEMA`. A dedicated store error is the
  alternative (`Error::NotCanonical`); the test `put_class_rejects_bytes_that_are_not_canonical`
  has the one assertion to change.
- Strict suspension of a re-put of a stored class (see rule 6).
- A method name bound as a function (any hash) is rebound by `bind_class` rather than refused.
- `bind_class` does not require the parent or interfaces of the class to be stored (nothing
  in 16.3 asks for it); `put_class` does not either.
- `bind_class` with a class whose steward namespace is suspended but whose name's host is not
  is allowed (suspension is checked on the name's host); only `put_class` looks at the steward.
- There is no bindings listing in the store, so "mark each row as function or class" has
  nothing to apply to; `resolve_class_binding` next to `resolve_binding` is the answer for 9.7.

## Running

The tests use the repo's usual setup (`tools/cargo.sh 1.99 test --workspace`, the
`datumesh-pg` container on the `datumesh` network), plus core 0.3.8 by a local patch, for
example `.cargo/config.toml` (not committed):

```toml
[patch.crates-io]
datumesh-core = { path = "/core/crates/datumesh-core" }   # mount core at /core in the container
```

The tests build class descriptors with `datumesh_core::define_class` (a two-key `fx_pair` with
methods `rate` and `spot`, and a subclass `fx_forward` with an inherited method set); they do
not use the spec vectors, which the store's `spec/` submodule (older than `spec-0.5.0`) does
not have.
