-- datumesh-store schema, migration 18 (0.7.0, CONTRACT-http-listing L3): the listings of another
-- mesh's functions, and the listed hosts. Additive: no stored row changes; no binding, namespace,
-- cell, receipt or version is touched by a listing.
--
-- listings: the present state only, one row per listed name (no history). The descriptor's bytes
-- are kept here as fetched; the same descriptor is also in descriptors (so /descriptor/<hash>
-- answers it). subjects is the canonical JSON array of [domain, thing, measure, region].
CREATE TABLE listings (
    name text COLLATE "C" PRIMARY KEY CHECK (octet_length(name) BETWEEN 1 AND 512),
    host text COLLATE "C" NOT NULL CHECK (octet_length(host) BETWEEN 1 AND 253),
    fn_hash text COLLATE "C" NOT NULL CHECK (fn_hash ~ '^[0-9a-f]{64}$') REFERENCES descriptors (fn_hash),
    subjects text NOT NULL CHECK (octet_length(subjects) <= 65536),
    descriptor bytea NOT NULL,
    key_id text NOT NULL CHECK (octet_length(key_id) BETWEEN 1 AND 256),
    label text NOT NULL CHECK (octet_length(label) <= 120),
    description text NOT NULL CHECK (octet_length(description) BETWEEN 1 AND 1024),
    fetched_at text COLLATE "C" NOT NULL CHECK (fetched_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$'),
    listed_at text COLLATE "C" NOT NULL CHECK (listed_at ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$')
);

CREATE INDEX listings_host_name ON listings (host, name);

-- listed_hosts: one row per listed host. last_seen is the day of the last successful fetch (kept
-- once per host, written only when it moves to another day, or when the host returns); unreachable_since
-- is the instant of the first failed run after the last success (empty while reachable).
CREATE TABLE listed_hosts (
    host text COLLATE "C" PRIMARY KEY CHECK (octet_length(host) BETWEEN 1 AND 253),
    last_seen text COLLATE "C" CHECK (last_seen IS NULL OR last_seen ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'),
    unreachable_since text COLLATE "C" CHECK (unreachable_since IS NULL
        OR unreachable_since ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{6}Z$')
);

-- catalog_entries.source gains 'listed' (signed_by is the listed host).
ALTER TABLE catalog_entries DROP CONSTRAINT catalog_entries_source_check;
ALTER TABLE catalog_entries ADD CONSTRAINT catalog_entries_source_check CHECK (source IN ('publisher', 'operator', 'listed'));
