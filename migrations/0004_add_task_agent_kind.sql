ALTER TABLE tasks ADD COLUMN agent_kind TEXT CHECK (agent_kind IS NULL OR agent_kind IN ('claude'));
