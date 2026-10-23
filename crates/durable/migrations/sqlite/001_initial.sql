CREATE TABLE ouro_revision (id INTEGER PRIMARY KEY CHECK (id = 1), revision INTEGER NOT NULL);
INSERT INTO ouro_revision VALUES (1, 0);
-- Rows are versioned run aggregates or definition catalogs. Records inside a
-- run are ordered by invocation key; cross-run changes share one transaction.
CREATE TABLE ouro_rows (kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (kind, id));
