-- Dead ends where money and orders disagree, captured as they happen so the
-- admin "needs attention" list can be a query rather than a log scrape.
--
-- A normal settled payment ends with the prefix's status changing to 'paid',
-- which leaves a trace in the prefixes table. When a settled payment lands on
-- a prefix that cannot take it -- cancelled, a draft, deleted, or already past
-- printing -- there is no status change to record it, so the webhook writes a
-- row here instead. Every row says what a human should do about it.
--
-- The deleted-prefix case is exactly why this table exists: the prefix row is
-- gone (cascade), so without this the money would exist only in a WARN log
-- line. The `prefix` column is kept even when the prefixes row is gone.
CREATE TABLE IF NOT EXISTS payment_anomalies (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    prefix       TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    message      TEXT    NOT NULL,
    action       TEXT    NOT NULL,
    session_id   TEXT,
    amount_cents INTEGER,
    currency     TEXT,
    created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- The list reads newest-first, and a prefix's an anomaly only needs clearing
-- when it ages out of view; this keeps that ordering cheap.
CREATE INDEX IF NOT EXISTS idx_payment_anomalies_created ON payment_anomalies (created_at DESC);
