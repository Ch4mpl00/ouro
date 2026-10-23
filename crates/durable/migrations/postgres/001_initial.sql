-- C collation gives prefix/range scans the same UTF-8 order as the engine.
-- The composite primary-key B-tree indexes every engine-owned lookup.
CREATE TABLE ouro_rows (
    kind TEXT NOT NULL COLLATE "C",
    id TEXT NOT NULL COLLATE "C",
    value TEXT NOT NULL,
    PRIMARY KEY (kind, id)
);
