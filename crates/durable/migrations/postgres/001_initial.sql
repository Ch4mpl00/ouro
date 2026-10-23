CREATE TABLE ouro_revision (id INTEGER PRIMARY KEY CHECK (id = 1), revision BIGINT NOT NULL);
INSERT INTO ouro_revision VALUES (1, 0);
CREATE TABLE ouro_rows (kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (kind, id));
