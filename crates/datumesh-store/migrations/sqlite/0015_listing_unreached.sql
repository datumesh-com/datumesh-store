-- datumesh-store SQLite schema, migration 15 (0.7.2, CONTRACT-http-listing, the re-review's N1; the
-- counterpart of PostgreSQL's migration 19). listings.unreached_since is the instant the listing was
-- first not reached by a run that read its host's catalogue; NULL while it is reached. Additive.
ALTER TABLE listings ADD COLUMN unreached_since TEXT CHECK (unreached_since IS NULL OR unreached_since GLOB
    '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9].[0-9][0-9][0-9][0-9][0-9][0-9]Z');
