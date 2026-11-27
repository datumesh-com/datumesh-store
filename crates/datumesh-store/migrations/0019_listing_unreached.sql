-- datumesh-store schema, migration 19 (0.7.2, CONTRACT-http-listing, the re-review's N1): a listed
-- name's own clock. unreached_since is the instant the listing was first not reached by a run that
-- read its host's catalogue; NULL while it is reached. Additive: nullable, no other column changes.
ALTER TABLE listings ADD COLUMN unreached_since text COLLATE "C" CHECK (unreached_since IS NULL
    OR unreached_since ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$');
