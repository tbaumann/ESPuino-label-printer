-- Ribbon availability: two stock flags, not three.
--
-- A print mode is offered while the ribbon it needs is in stock. There are two
-- ribbons — YMCKO (single-sided, and double-sided colour, which burns two panel
-- sets per card but is the same ribbon) and YMCKOKO (the black-back mode) — so
-- availability is a property of the *ribbon*, not of the mode: one flag per
-- ribbon, and a mode is gated on its ribbon. Single-sided and double-sided
-- colour therefore go out of stock together.
--
-- Stored in the database like mode_prices, so the operator can pull a ribbon
-- from the shop without a redeploy. Both seeded in stock, so nothing changes
-- for an existing deployment until someone flips a flag.
CREATE TABLE ribbon_stock (
    ribbon   TEXT PRIMARY KEY,
    in_stock INTEGER NOT NULL
);

INSERT INTO ribbon_stock (ribbon, in_stock) VALUES
    ('ymcko', 1),
    ('ymckoko', 1);
