-- Prefixes: a customer order / label batch. The uuid is picked by the
-- customer or assigned by the server.
CREATE TABLE IF NOT EXISTS prefixes (
    uuid       TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Cards: one printable card (front image + optional black/white back text).
-- The id is an index within its prefix, not global.
CREATE TABLE IF NOT EXISTS cards (
    prefix_uuid        TEXT    NOT NULL REFERENCES prefixes(uuid) ON DELETE CASCADE,
    id                 INTEGER NOT NULL,
    name               TEXT    NOT NULL,
    serial_number      TEXT,
    back_text          TEXT,
    image_path         TEXT    NOT NULL,
    image_content_type TEXT    NOT NULL,
    created_at         TEXT    NOT NULL DEFAULT (datetime('now')),
    updated_at         TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (prefix_uuid, id)
);

CREATE INDEX IF NOT EXISTS idx_cards_prefix ON cards (prefix_uuid);