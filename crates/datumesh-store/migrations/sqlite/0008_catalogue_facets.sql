-- datumesh-store SQLite schema, migration 8 (additions only; a store release later; PostgreSQL's
-- migration 12; CONTRACT-classes-round C2 and K9): facets and extends on catalogue entries.
--
-- See migrations/0012_catalogue_facets.sql for what the two columns are. facets is JSON text, a
-- canonical object of name lists; an entry without facets has '{}'. extends is NULL when the
-- function extends no class.

ALTER TABLE catalog_entries ADD COLUMN facets TEXT NOT NULL DEFAULT '{}'
    CHECK (length(CAST(facets AS BLOB)) <= 65536 AND json_valid(facets) AND json_type(facets) = 'object');

ALTER TABLE catalog_entries ADD COLUMN extends TEXT
    CHECK (extends IS NULL OR length(CAST(extends AS BLOB)) BETWEEN 1 AND 512);
