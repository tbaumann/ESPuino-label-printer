-- Prefix lifecycle: draft -> submitted -> paid -> queued -> printing -> printed
-- (cancelled is reachable from any pre-printing state).
-- Cards may be modified while status is draft/submitted/paid/queued; once
-- printing starts the prefix is frozen.
ALTER TABLE prefixes ADD COLUMN status TEXT NOT NULL DEFAULT 'draft';
ALTER TABLE prefixes ADD COLUMN status_changed_at TEXT;
ALTER TABLE prefixes ADD COLUMN order_ref TEXT;

CREATE INDEX IF NOT EXISTS idx_prefixes_status ON prefixes (status);