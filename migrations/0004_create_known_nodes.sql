-- The nodes a client has met, and the key each address answered with.
--
-- Keyed by the address a person typed rather than by the key, because that is
-- what "remembered" means: the name you use for a machine. Two addresses that
-- reach the same node are two entries, and a substitution is one address
-- answering with a different key than it did before.
CREATE TABLE known_nodes (
    address TEXT PRIMARY KEY NOT NULL,
    id TEXT NOT NULL,
    label TEXT NOT NULL,
    met_at_ms INTEGER NOT NULL
);
