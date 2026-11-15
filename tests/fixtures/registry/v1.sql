-- Schema version 1 of a new token registry (accounts.sqlite3): sqlite_master in
-- creation order. Written from the registry `host token add` creates; never edit
-- the snapshot of a released version.
CREATE TABLE client_tokens (
    hash TEXT PRIMARY KEY NOT NULL CHECK(length(hash)=64 AND hash NOT GLOB '*[^0-9a-f]*'),
    name TEXT NOT NULL CHECK(length(name) BETWEEN 1 AND 64),
    scope TEXT NOT NULL CHECK(scope IN ('write','read')),
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    expires_at INTEGER,
    revoked_at INTEGER
);
CREATE UNIQUE INDEX client_tokens_live_name ON client_tokens(name) WHERE revoked_at IS NULL;
CREATE TABLE settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
