-- datumesh-store schema, migration 9 (additions only; a store release later): the catalogue
-- (CONTRACT-build2-part2-read R1): the entries that describe functions and classes for
-- search, the dictionaries of key values with their labels and aliases, the embeddings of
-- their text (only where the `vector` extension is available), and the log of find misses.
--
-- catalog_entries: one row per (kind, name). An operator's entry never replaces a publisher's
-- (the store checks that; the table only refuses what the bounds refuse). Mutable: an upsert
-- replaces the row. The text is bounded here, in bytes (the store checks first and answers
-- with the typed error). args and examples are JSON text, canonical (JCS) as the store writes
-- them.
--
-- key_values: the dictionary of a key of an owner (a function or class name): each value of
-- the key with its canonical lexical form, a label and aliases from the publisher's
-- dictionary. n_cells is the number of cells of the owner's function that have the value
-- for the key, refreshed by the store (refresh_value_counts); an upsert does not touch it.
--
-- embeddings: 384-dimensional vectors (model, then ref_kind and ref_id; ref_id is the id of
-- the catalog entry or of the key value that ref_kind names). Created only when the `vector`
-- extension is available (pg_available_extensions), with an HNSW index on cosine distance.
-- Where it is not, there is no table and the store reports vector_available() == false.
--
-- find_misses: a query that found nothing, and when. Text only: no IP, no key, no user. The
-- store keeps at most 100,000 rows and prunes the oldest.
--
-- Trigram indexes (pg_trgm) on the labels, values and descriptions are made where pg_trgm
-- is available: the text search matches with LIKE on lower(), which they serve.

CREATE TABLE catalog_entries (
    id bigserial PRIMARY KEY,
    kind text NOT NULL CHECK (kind IN ('class', 'method', 'function')),
    name text NOT NULL CHECK (octet_length(name) BETWEEN 1 AND 512),
    -- The function hash (kind function) or the class hash (kind class), when it has one.
    hash text CHECK (hash IS NULL OR hash ~ '^[0-9a-f]{64}$'),
    label text NOT NULL CHECK (octet_length(label) <= 120),
    description text NOT NULL CHECK (octet_length(description) BETWEEN 1 AND 1024),
    args text NOT NULL DEFAULT '{}' CHECK (octet_length(args) <= 65536),
    examples text NOT NULL DEFAULT '[]' CHECK (octet_length(examples) <= 65536),
    source text NOT NULL CHECK (source IN ('publisher', 'operator')),
    signed_by text CHECK (signed_by IS NULL OR octet_length(signed_by) BETWEEN 1 AND 256),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (kind, name)
);

CREATE TABLE key_values (
    id bigserial NOT NULL UNIQUE,
    owner text NOT NULL CHECK (octet_length(owner) BETWEEN 1 AND 512),
    key text NOT NULL CHECK (octet_length(key) BETWEEN 1 AND 128),
    value text NOT NULL CHECK (octet_length(value) BETWEEN 1 AND 1024),
    label text NOT NULL DEFAULT '' CHECK (octet_length(label) <= 120),
    aliases text[] NOT NULL DEFAULT '{}',
    n_cells bigint NOT NULL DEFAULT 0 CHECK (n_cells >= 0),
    PRIMARY KEY (owner, key, value)
);

CREATE TABLE find_misses (
    query text NOT NULL CHECK (octet_length(query) BETWEEN 1 AND 512),
    at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX find_misses_at ON find_misses (at);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'pg_trgm') THEN
        CREATE EXTENSION IF NOT EXISTS pg_trgm;
        CREATE INDEX catalog_entries_label_trgm ON catalog_entries USING gin (lower(label) gin_trgm_ops);
        CREATE INDEX catalog_entries_description_trgm ON catalog_entries USING gin (lower(description) gin_trgm_ops);
        CREATE INDEX key_values_value_trgm ON key_values USING gin (lower(value) gin_trgm_ops);
        CREATE INDEX key_values_label_trgm ON key_values USING gin (lower(label) gin_trgm_ops);
    END IF;
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'vector') THEN
        CREATE EXTENSION IF NOT EXISTS vector;
        CREATE TABLE embeddings (
            ref_kind text NOT NULL CHECK (ref_kind IN ('entry', 'value')),
            ref_id bigint NOT NULL CHECK (ref_id > 0),
            model text NOT NULL CHECK (octet_length(model) BETWEEN 1 AND 200),
            vec vector(384) NOT NULL,
            PRIMARY KEY (ref_kind, ref_id, model)
        );
        CREATE INDEX embeddings_vec ON embeddings USING hnsw (vec vector_cosine_ops);
    END IF;
END
$$;
