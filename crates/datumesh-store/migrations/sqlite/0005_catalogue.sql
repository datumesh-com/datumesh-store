-- datumesh-store SQLite schema, migration 5 (additions only; PostgreSQL's migration 9): the
-- catalogue (CONTRACT-build2-part2-read R1). See migrations/0009_catalogue.sql for what each
-- table is for. No embeddings: SQLite has no vector extension here, so the store reports
-- vector_available() == false.
--
-- catalog_entries and key_values are mutable (an upsert replaces the row); find_misses is
-- append-only in practice (the store only adds and prunes the oldest).

CREATE TABLE catalog_entries (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('class', 'method', 'function')),
    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 512),
    hash TEXT CHECK (hash IS NULL OR (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*')),
    label TEXT NOT NULL CHECK (length(CAST(label AS BLOB)) <= 120),
    description TEXT NOT NULL CHECK (length(CAST(description AS BLOB)) BETWEEN 1 AND 1024),
    args TEXT NOT NULL DEFAULT '{}' CHECK (length(CAST(args AS BLOB)) <= 65536),
    examples TEXT NOT NULL DEFAULT '[]' CHECK (length(CAST(examples AS BLOB)) <= 65536),
    source TEXT NOT NULL CHECK (source IN ('publisher', 'operator')),
    signed_by TEXT CHECK (signed_by IS NULL OR length(CAST(signed_by AS BLOB)) BETWEEN 1 AND 256),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (kind, name)
) STRICT;

CREATE TABLE key_values (
    id INTEGER PRIMARY KEY,
    owner TEXT NOT NULL CHECK (length(CAST(owner AS BLOB)) BETWEEN 1 AND 512),
    key TEXT NOT NULL CHECK (length(CAST(key AS BLOB)) BETWEEN 1 AND 128),
    value TEXT NOT NULL CHECK (length(CAST(value AS BLOB)) BETWEEN 1 AND 1024),
    label TEXT NOT NULL DEFAULT '' CHECK (length(CAST(label AS BLOB)) <= 120),
    -- A JSON array of strings (canonical), from the publisher's dictionary only.
    aliases TEXT NOT NULL DEFAULT '[]',
    n_cells INTEGER NOT NULL DEFAULT 0 CHECK (n_cells >= 0),
    UNIQUE (owner, key, value)
) STRICT;

CREATE TABLE find_misses (
    query TEXT NOT NULL CHECK (length(CAST(query AS BLOB)) BETWEEN 1 AND 512),
    at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
) STRICT;

CREATE INDEX find_misses_at ON find_misses (at);
