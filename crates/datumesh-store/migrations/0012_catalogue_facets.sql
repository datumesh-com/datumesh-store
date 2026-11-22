-- datumesh-store schema, migration 12 (additions only; a store release later; CONTRACT-classes-round
-- C2 and K9): facets and extends on catalogue entries.
--
-- facets: the facets of an entry, a JSON object of name lists (canonical JCS, as the store writes
-- it): a key among domain, thing, measure, region and frequency, each a list of names. The store
-- checks the form; this table refuses only what is not a JSON object, and a value over 65536 bytes.
-- An entry without facets has '{}'. Rows that exist before this migration get '{}'.
--
-- extends: the full name of the class a function extends (spec 16.8), NULL when it extends none.
-- The publisher's statement, outside every hash, as the rest of the catalogue. Only a function
-- extends a class: the store checks that, so that both backends keep the same columns.

ALTER TABLE catalog_entries ADD COLUMN facets text NOT NULL DEFAULT '{}'
    CHECK (octet_length(facets) <= 65536 AND jsonb_typeof(facets::jsonb) = 'object');

ALTER TABLE catalog_entries ADD COLUMN extends text
    CHECK (extends IS NULL OR octet_length(extends) BETWEEN 1 AND 512);
