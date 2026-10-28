-- The black-back ribbon is YMCKOK, not YMCKOKO.
--
-- The service has called it YMCKOKO since the ribbon was introduced, a spelling
-- the ribbon's own label does not carry: YMCKOK is the colour panels plus a
-- black panel for the back. Only the NAME changes here — the panel set, the
-- depth a back is rendered in and the billing unit are the same ribbon.
--
-- The earlier migrations keep their spelling, and must: sqlx stores a checksum
-- per applied migration and refuses to run a database whose migration file has
-- since been edited (MigrateError::VersionMismatch), so renaming the value in
-- `0010`/`0012` would break every deployment that has already applied them.
-- `prefixes.ribbon`'s column DEFAULT in `0012` stays for the same reason; every
-- INSERT names the column (`db::create_prefix`), so that default is unreachable.
UPDATE prefixes SET ribbon = 'ymckok' WHERE ribbon = 'ymckoko';
UPDATE ribbon_stock SET ribbon = 'ymckok' WHERE ribbon = 'ymckoko';
UPDATE ribbon_prices SET ribbon = 'ymckok' WHERE ribbon = 'ymckoko';
