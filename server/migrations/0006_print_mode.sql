-- Print modes: one ribbon-pass scheme per prefix.
--
-- colour_front_black_back (YMCKOKO) is the mode every pre-existing row had
-- behaviour for — front in colour, back as black-only text — so it is the
-- default and existing rows keep their meaning.
--
--   single_sided         YMCKO, one panel set/card: front only, no back at all.
--   double_sided_color   YMCKO, two panel sets/card: colour front AND colour back.
--   colour_front_black_back  YMCKOKO, colour front + black-only text back (default).
ALTER TABLE prefixes ADD COLUMN print_mode TEXT NOT NULL DEFAULT 'color_front_black_back';

-- A colour back (double_sided_color) needs its own image, which the card never
-- carried before — it only had a front image + black back text. NULL for every
-- mode with no colour back. The path is never serialized to clients, mirroring
-- the front image_path.
ALTER TABLE cards ADD COLUMN back_image_path TEXT;
ALTER TABLE cards ADD COLUMN back_image_content_type TEXT;

-- The spooler walks a prefix's cards; keep finding a prefix's cards by mode
-- cheap once pricing and ribbon checks read it.
CREATE INDEX IF NOT EXISTS idx_prefixes_print_mode ON prefixes (print_mode);
