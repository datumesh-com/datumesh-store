-- datumesh-store schema, migration 5 (additions only; a store release later).
--
-- namespaces.consecutive_failures, namespaces.last_checked_at: the outcome of the checks of a
-- host's key document (domain validation). A failed check adds one to the count, a successful
-- one resets it to zero; `last_checked_at` is the time of the latest check, either way.
-- `suspended_at` (already there) stays the single source of truth for suspension: set by the
-- failure that reaches the threshold, cleared by the next success.

ALTER TABLE namespaces ADD COLUMN consecutive_failures integer NOT NULL DEFAULT 0
    CHECK (consecutive_failures >= 0);
ALTER TABLE namespaces ADD COLUMN last_checked_at text COLLATE "C"
    CHECK (last_checked_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$');
ALTER TABLE namespaces ADD COLUMN last_checked_at_at timestamptz
    CHECK (last_checked_at_at IS NOT DISTINCT FROM datumesh_instant(last_checked_at));
