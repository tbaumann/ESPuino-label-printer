-- Admin UI sessions, persisted so a restart does not log the operator out.
-- There is no account system to log back in with, so losing a session on
-- restart would mean re-entering the token by hand.
-- expires_at is a unix epoch in seconds. Expired rows are pruned lazily when
-- a new session is created; a lookup simply ignores them.
CREATE TABLE IF NOT EXISTS admin_sessions (
    id         TEXT    PRIMARY KEY,
    expires_at INTEGER NOT NULL
);
