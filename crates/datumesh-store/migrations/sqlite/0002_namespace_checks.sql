-- datumesh-store SQLite schema, migration 2 (additions only; PostgreSQL's migration 5): the
-- outcome of the checks of a host's key document (domain validation). See
-- migrations/0005_namespace_checks.sql.

ALTER TABLE namespaces ADD COLUMN consecutive_failures INTEGER NOT NULL DEFAULT 0
    CHECK (consecutive_failures >= 0);
ALTER TABLE namespaces ADD COLUMN last_checked_at TEXT
    CHECK (last_checked_at IS NULL OR last_checked_at GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z');
