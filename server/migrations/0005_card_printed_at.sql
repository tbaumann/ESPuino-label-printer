-- Per-card print state, for the future print spooler.
--
-- A card is marked printed only once the printer confirms ejection; the
-- timestamp lives here (not in spooler state) so a restart can resume a
-- half-finished prefix without reprinting a batch. NULL = not yet printed.
--
-- Operator-only state: it is never serialized into the public Card JSON, so
-- neither the customer view nor the admin print preview can see it.
ALTER TABLE cards ADD COLUMN printed_at TEXT;

-- The spooler walks a prefix's unprinted cards; this makes that walk cheap.
CREATE INDEX IF NOT EXISTS idx_cards_printed ON cards (prefix_uuid, printed_at);
