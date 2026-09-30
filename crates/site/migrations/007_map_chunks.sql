CREATE TABLE map_chunks (
    cx INTEGER NOT NULL,
    cy INTEGER NOT NULL,
    z SMALLINT NOT NULL,
    tiles JSONB NOT NULL,
    PRIMARY KEY (cx, cy, z)
);
