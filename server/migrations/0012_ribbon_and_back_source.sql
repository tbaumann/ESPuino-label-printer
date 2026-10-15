-- Two choices, at the two layers that actually make them.
--
-- The old model had one three-way `print_mode` per prefix and read the back
-- side off it. That conflated things that are decided in different places:
--
--   * which ribbon the *job* is printed with (YMCKO / YMCKOKO) — a per-job
--     choice, because a batch cannot mix ribbon loads, and the only thing whose
--     availability is a stock question; and
--   * what a *card's back side* is (nothing at all, a copy of the front, a
--     second uploaded image, or markdown) — a per-card choice, because one
--     order may legitimately contain both single- and double-sided cards.
--
-- So `prefixes.print_mode` becomes `prefixes.ribbon`, and each card carries a
-- `back_source`. The colour a back is rendered in is no longer stored anywhere:
-- it is the ribbon's consequence (YMCKO's back is a colour panel set, YMCKOKO's
-- is the second black K panel), which is why `image`/`front` backs are refused
-- on a YMCKOKO prefix.
--
-- Pricing follows the same split. A price is now the price of one printed
-- *side* on a ribbon: YMCKO bills one side for a single-sided card and two for
-- a double-sided one, YMCKOKO always bills two (the second black panel is
-- consumed either way). `mode_prices` therefore becomes `ribbon_prices`, keyed
-- by ribbon.

-- --------------------------------------------------------------------------
-- prefixes: print_mode -> ribbon
-- --------------------------------------------------------------------------

ALTER TABLE prefixes ADD COLUMN ribbon TEXT NOT NULL DEFAULT 'ymckoko';

-- Every pre-existing row printed in one of the three old modes. Both YMCKO
-- modes (`single_sided`, `double_sided_color`) become the same ribbon, which is
-- exactly the collapse this migration is for: the single/double choice moves
-- onto the cards below.
UPDATE prefixes SET ribbon = CASE print_mode
    WHEN 'color_front_black_back' THEN 'ymckoko'
    ELSE 'ymcko'
END;

DROP INDEX IF EXISTS idx_prefixes_print_mode;
ALTER TABLE prefixes DROP COLUMN print_mode;

CREATE INDEX IF NOT EXISTS idx_prefixes_ribbon ON prefixes (ribbon);

-- The ribbon that was actually charged. Same collapse, one row at a time: an
-- unfrozen (NULL) paid mode stays NULL.
ALTER TABLE prefixes ADD COLUMN paid_ribbon TEXT;
UPDATE prefixes SET paid_ribbon = CASE paid_print_mode
    WHEN 'color_front_black_back' THEN 'ymckoko'
    WHEN 'single_sided' THEN 'ymcko'
    WHEN 'double_sided_color' THEN 'ymcko'
    ELSE NULL
END;
ALTER TABLE prefixes DROP COLUMN paid_print_mode;

-- --------------------------------------------------------------------------
-- pricing: per mode -> per ribbon, and per card -> per printed side
-- --------------------------------------------------------------------------

-- The old `single_sided` price was the price of one printed side; the old
-- `double_sided_color` price was the price of a whole two-sided card, which is
-- now just two of them. So the per-side price is the old single-sided one, and
-- the old double-sided figure is superseded rather than migrated. A deployment
-- that had priced the two modes differently should re-check `ribbon_prices`
-- after this migration.
--
-- Restore the row first if a hand-edited database has lost it: a missing price
-- is a misconfiguration the checkout refuses, and it should not become one here
-- by accident.
INSERT OR IGNORE INTO mode_prices (print_mode, price_cents)
    SELECT 'single_sided', 500
    WHERE NOT EXISTS (
        SELECT 1 FROM mode_prices WHERE print_mode IN ('single_sided', 'double_sided_color')
    );

DELETE FROM mode_prices WHERE print_mode = 'double_sided_color';
UPDATE mode_prices SET print_mode = 'ymcko' WHERE print_mode = 'single_sided';
UPDATE mode_prices SET print_mode = 'ymckoko' WHERE print_mode = 'color_front_black_back';

ALTER TABLE mode_prices RENAME TO ribbon_prices;
ALTER TABLE ribbon_prices RENAME COLUMN print_mode TO ribbon;

-- --------------------------------------------------------------------------
-- cards: the back side becomes an explicit, per-card choice
-- --------------------------------------------------------------------------

ALTER TABLE cards ADD COLUMN back_source TEXT NOT NULL DEFAULT 'none';

-- Read the old back off what the row actually carried. The old modes made this
-- unambiguous: a colour back image could only exist in a `double_sided_color`
-- prefix and markdown only in a `color_front_black_back` one, so a row has at
-- most one of the two. A row with neither is a single-sided card.
UPDATE cards SET back_source = CASE
    WHEN back_image_content_type IS NOT NULL THEN 'image'
    WHEN back_text IS NOT NULL AND trim(back_text) <> '' THEN 'markdown'
    ELSE 'none'
END;
