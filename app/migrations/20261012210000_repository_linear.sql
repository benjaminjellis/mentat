CREATE TABLE repository_linear (
    repository TEXT PRIMARY KEY NOT NULL,
    workspace_id TEXT NOT NULL,
    workspace_name TEXT NOT NULL
);

CREATE TABLE repository_linear_oauth (
    repository TEXT NOT NULL REFERENCES repository_linear(repository),
    client_id TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL CHECK (expires_at >= 0),
    PRIMARY KEY (repository, client_id)
);
