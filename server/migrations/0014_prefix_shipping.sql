-- Where a paid order ships, as Stripe Checkout collected it.
--
-- Written by the Stripe webhook out of the settled session, and read only by
-- the admin endpoint that serves it. It is the customer's address, so it is
-- deliberately NOT part of the public prefix JSON: the customer already knows
-- where they live, and the prefix uuid is the only credential there is.
--
-- Every column is nullable and they are all set together: Stripe collected no
-- address at all (a deployment whose session carries no
-- `shipping_address_collection`), or the payment has not settled yet. Nothing
-- is ever stored half-blank -- the webhook drops an all-empty address rather
-- than writing it, so a blank payload cannot overwrite a real one.
ALTER TABLE prefixes ADD COLUMN shipping_name TEXT;
ALTER TABLE prefixes ADD COLUMN shipping_line1 TEXT;
ALTER TABLE prefixes ADD COLUMN shipping_line2 TEXT;
ALTER TABLE prefixes ADD COLUMN shipping_postal_code TEXT;
ALTER TABLE prefixes ADD COLUMN shipping_city TEXT;
ALTER TABLE prefixes ADD COLUMN shipping_state TEXT;
ALTER TABLE prefixes ADD COLUMN shipping_country TEXT;
