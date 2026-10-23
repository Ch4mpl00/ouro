-- Engine-owned primary and secondary keys share atomic multi-key commits.
CREATE TABLE ouro_rows (
    kind TEXT NOT NULL COLLATE BINARY,
    id TEXT NOT NULL COLLATE BINARY,
    value TEXT NOT NULL,
    PRIMARY KEY (kind, id)
) WITHOUT ROWID;
