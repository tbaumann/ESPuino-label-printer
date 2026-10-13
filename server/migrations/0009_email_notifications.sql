-- One row per email we are allowed to send exactly once.
--
-- The Stripe webhook is what triggers the order confirmation, and Stripe
-- retries a delivery until it is answered 2xx -- so the naive version mails the
-- customer three times for one payment. The row here is the claim: it is
-- inserted *before* the send, under a key derived from the event that caused it
-- (for the paid confirmation, `<kind>:<stripe session id>`), and a duplicate or
-- concurrent delivery loses the claim and sends nothing. That is the whole
-- exactly-once mechanism, and it is deliberately a database key rather than
-- in-process state, so it survives a restart in the middle of Stripe's retries.
--
-- It is also the audit trail for what was mailed: `status` is the outcome of
-- the attempt, `error` the reason when it failed. A failed row keeps its claim
-- on purpose -- resending is the operator's call (the recipient is stored for
-- exactly that), never an automatic retry that could double-send.
CREATE TABLE IF NOT EXISTS email_notifications (
    dedupe_key  TEXT    PRIMARY KEY,
    kind        TEXT    NOT NULL,
    prefix      TEXT    NOT NULL,
    recipient   TEXT    NOT NULL,
    session_id  TEXT,
    -- 'pending' (claimed, not sent yet), 'sent' or 'failed'.
    status      TEXT    NOT NULL DEFAULT 'pending',
    provider_id TEXT,
    error       TEXT,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
    sent_at     TEXT
);

-- The list reads newest-first when someone goes looking for what was mailed.
CREATE INDEX IF NOT EXISTS idx_email_notifications_created
    ON email_notifications (created_at DESC);

-- Rows of one prefix, for the same reason.
CREATE INDEX IF NOT EXISTS idx_email_notifications_prefix
    ON email_notifications (prefix);
