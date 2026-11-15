-- datumesh-store schema, migration 7 (additions only; a store release later): the audit
-- column written_at on the two binding tables (CONTRACT-1.8b S3).
--
-- written_at: when the row was written, filled by the database at insert (DEFAULT now(),
-- set after the column is added, so that the rows that existed before this migration stay
-- NULL: ADD COLUMN with the default would have filled them with the migration's time). It is
-- in no hash and read by no API: an audit record only. The append-only triggers of the two
-- tables are untouched.

ALTER TABLE name_bindings ADD COLUMN written_at timestamptz;
ALTER TABLE name_bindings ALTER COLUMN written_at SET DEFAULT now();

ALTER TABLE class_bindings ADD COLUMN written_at timestamptz;
ALTER TABLE class_bindings ALTER COLUMN written_at SET DEFAULT now();
