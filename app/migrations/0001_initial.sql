CREATE TABLE workspaces (
    repository TEXT NOT NULL,
    ticket_id TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    cleanup INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (repository, ticket_id)
);

CREATE TABLE app_state (
    repository TEXT PRIMARY KEY,
    selected_ticket TEXT
);

CREATE TABLE agent_sessions (
    repository TEXT NOT NULL,
    ticket_id TEXT NOT NULL,
    agent TEXT NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (repository, ticket_id, agent),
    FOREIGN KEY (repository, ticket_id) REFERENCES workspaces(repository, ticket_id) ON DELETE CASCADE
);
