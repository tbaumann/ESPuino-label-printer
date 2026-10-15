-- Server-side card render pipeline: the four derived artifacts each carry a
-- render status the API reports (pending | ready | failed). The status lives
-- in the DB because the UI and the print queue query it; the bytes live as
-- sibling blobs under data/blobs/<prefix>/.
--
-- Storage naming convention (one renderer owns the truth; a complete card's
-- statuses are ready once writing has returned, because rendering is
-- synchronous — no worker loop):
--   {id}-display.png      front_display
--   {id}-back-display.png back_display
--   {id}-front-print.bmp  front_raster
--   {id}-back-print.bmp   back_raster

ALTER TABLE cards ADD COLUMN front_display_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE cards ADD COLUMN front_display_error TEXT;
ALTER TABLE cards ADD COLUMN back_display_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE cards ADD COLUMN back_display_error TEXT;
ALTER TABLE cards ADD COLUMN front_raster_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE cards ADD COLUMN front_raster_error TEXT;
ALTER TABLE cards ADD COLUMN back_raster_status TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE cards ADD COLUMN back_raster_error TEXT;
