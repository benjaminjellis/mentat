CREATE TABLE ticket_forks (
    repository TEXT NOT NULL,
    ticket_id TEXT NOT NULL,
    source_session TEXT NOT NULL,
    revision TEXT NOT NULL,
    PRIMARY KEY (repository, ticket_id)
);
