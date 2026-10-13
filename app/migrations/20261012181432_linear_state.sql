CREATE TABLE linear_cache (
    identity TEXT PRIMARY KEY NOT NULL,
    snapshot TEXT NOT NULL
);

CREATE TABLE linear_oauth (
    client_id TEXT PRIMARY KEY NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at >= 0)
);
