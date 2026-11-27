-- datumesh-store SQLite schema, migration 14 (0.7.0, CONTRACT-http-listing L3; the counterpart of
-- PostgreSQL's migration 18). Additive: no stored row changes; no binding, namespace, cell, receipt
-- or version is touched by a listing. See migrations/0018_listings.sql for what each table is for.
--
-- catalog_entries.source gains 'listed'. SQLite cannot alter a CHECK, so the table is rebuilt with the
-- same columns, keys, checks and index, every row copied as it is (id and updated_at included). Nothing
-- references catalog_entries, and no trigger is on it.

CREATE TABLE catalog_entries_new (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('class', 'method', 'function')),
    name TEXT NOT NULL CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 512),
    hash TEXT CHECK (hash IS NULL OR (length(hash) = 64 AND hash NOT GLOB '*[^0-9a-f]*')),
    label TEXT NOT NULL CHECK (length(CAST(label AS BLOB)) <= 120),
    description TEXT NOT NULL CHECK (length(CAST(description AS BLOB)) BETWEEN 1 AND 1024),
    args TEXT NOT NULL DEFAULT '{}' CHECK (length(CAST(args AS BLOB)) <= 65536),
    examples TEXT NOT NULL DEFAULT '[]' CHECK (length(CAST(examples AS BLOB)) <= 65536),
    source TEXT NOT NULL CHECK (source IN ('publisher', 'operator', 'listed')),
    signed_by TEXT CHECK (signed_by IS NULL OR length(CAST(signed_by AS BLOB)) BETWEEN 1 AND 256),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    facets TEXT NOT NULL DEFAULT '{}'
        CHECK (length(CAST(facets AS BLOB)) <= 65536 AND json_valid(facets) AND json_type(facets) = 'object'),
    extends TEXT CHECK (extends IS NULL OR length(CAST(extends AS BLOB)) BETWEEN 1 AND 512),
    source_name TEXT,
    UNIQUE (kind, name)
) STRICT;
INSERT INTO catalog_entries_new (id, kind, name, hash, label, description, args, examples, source, signed_by,
                                 updated_at, facets, extends, source_name)
    SELECT id, kind, name, hash, label, description, args, examples, source, signed_by,
           updated_at, facets, extends, source_name FROM catalog_entries;
DROP TABLE catalog_entries;
ALTER TABLE catalog_entries_new RENAME TO catalog_entries;
CREATE INDEX catalog_entries_extends ON catalog_entries (extends, name) WHERE extends IS NOT NULL;

CREATE TABLE listings (
    name TEXT PRIMARY KEY CHECK (length(CAST(name AS BLOB)) BETWEEN 1 AND 512),
    host TEXT NOT NULL CHECK (length(CAST(host AS BLOB)) BETWEEN 1 AND 253),
    fn_hash TEXT NOT NULL CHECK (length(fn_hash) = 64 AND fn_hash NOT GLOB '*[^0-9a-f]*')
        REFERENCES descriptors (fn_hash),
    subjects TEXT NOT NULL CHECK (length(CAST(subjects AS BLOB)) <= 65536 AND json_valid(subjects)),
    descriptor BLOB NOT NULL,
    key_id TEXT NOT NULL CHECK (length(CAST(key_id AS BLOB)) BETWEEN 1 AND 256),
    label TEXT NOT NULL CHECK (length(CAST(label AS BLOB)) <= 120),
    description TEXT NOT NULL CHECK (length(CAST(description AS BLOB)) BETWEEN 1 AND 1024),
    fetched_at TEXT NOT NULL CHECK (fetched_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z'),
    listed_at TEXT NOT NULL CHECK (listed_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z')
) STRICT;
CREATE INDEX listings_host_name ON listings (host, name);

CREATE TABLE listed_hosts (
    host TEXT PRIMARY KEY CHECK (length(CAST(host AS BLOB)) BETWEEN 1 AND 253),
    last_seen TEXT CHECK (last_seen IS NULL OR last_seen GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    unreachable_since TEXT CHECK (unreachable_since IS NULL OR unreachable_since GLOB
        '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z')
) STRICT;
