-- datumesh-store SQLite schema, migration 4 (additions only; PostgreSQL's migration 7): the
-- audit column written_at on the two binding tables (CONTRACT-1.8b S3). See
-- migrations/0007_binding_audit.sql.
--
-- written_at is a nullable text column with no default (SQLite cannot add a column whose
-- default is an expression): the insert statements of the store set it to
-- strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), and the rows that existed before this migration
-- stay NULL. It is in no hash and read by no API.

ALTER TABLE name_bindings ADD COLUMN written_at TEXT;
ALTER TABLE class_bindings ADD COLUMN written_at TEXT;
