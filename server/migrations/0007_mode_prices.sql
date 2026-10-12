-- Per-mode pricing in the database, not the environment.
--
-- One STRIPE_PER_CARD_CENTS priced every card the same. Now each print mode
-- has its own price, stored here so the shop config can serve it and the
-- checkout can sum over the cards' modes. Currency stays a single shop-wide
-- setting (STRIPE_CURRENCY) — a shop does not mix currencies, and Stripe
-- prices a whole session in one currency.
CREATE TABLE mode_prices (
    print_mode TEXT PRIMARY KEY,
    price_cents INTEGER NOT NULL
);

-- Seed every mode at the historical default (500) so existing deployments
-- keep their price until the operator changes it.
INSERT INTO mode_prices (print_mode, price_cents) VALUES
    ('single_sided', 500),
    ('double_sided_color', 500),
    ('color_front_black_back', 500);

-- The mode that was actually charged, snapshotted when the checkout session
-- is created (the moment the price is locked into Stripe). The spooler reads
-- this, not the prefix's current print_mode, so an operator changing the mode
-- after payment can never make the spooler print something the customer did
-- not pay for. NULL for prefixes that were never priced through checkout.
ALTER TABLE prefixes ADD COLUMN paid_print_mode TEXT;
