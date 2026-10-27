-- One physical card, one card row.
--
-- `serial_number` carries the card's RFID UID (four UID bytes as `%03d` each,
-- 12 decimal digits) once the print process has read the chip. A UID identifies
-- ONE physical card, and an external client (the MediaHub card-printing wizard)
-- promotes its own card per UID — so two cards of one prefix sharing a UID would
-- silently cost it a card.
--
-- Scoped to the prefix, so re-personalising the same chip in a second order
-- stays possible, and NULL (a card whose chip has not been read yet) is
-- excluded: any number of unprinted cards may share "no UID yet".
CREATE UNIQUE INDEX IF NOT EXISTS idx_cards_serial_number
    ON cards (prefix_uuid, serial_number) WHERE serial_number IS NOT NULL;
