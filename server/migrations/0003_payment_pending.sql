-- 'submitted' is gone. The customer's Pay button now moves a draft straight to
-- 'payment_pending' when it opens a Stripe Checkout session, and the signed
-- webhook moves that to 'paid'. There is no separate submit step, so nothing
-- can ever produce the old value again.
--
-- Existing rows have to be rewritten rather than merely tolerated: `status` is
-- parsed into a Rust enum on read, so a row still saying 'submitted' would fail
-- to load and the prefix would 500 instead of showing up.
UPDATE prefixes SET status = 'payment_pending' WHERE status = 'submitted';
