-- The key each remote address answered with, and what it called itself.
--
-- Keyed by the address a person typed rather than by the key, because that is
-- what "remembered" means: the name you use for a machine. Two addresses that
-- reach the same remote are two entries, and a substitution is one address
-- answering with a different key than it did before.
--
-- A row exists only once a remote has been met. Which addresses are *wanted* is
-- configuration, in `config.toml`; this table is only what each one has proved.
CREATE TABLE remote_keys (
    address TEXT PRIMARY KEY NOT NULL,
    key TEXT NOT NULL,
    label TEXT NOT NULL,
    seen_at_ms INTEGER NOT NULL
);
