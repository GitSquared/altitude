ALTER TABLE tasks ADD COLUMN custody TEXT NOT NULL DEFAULT 'autopilot'
    CHECK (custody IN ('autopilot', 'operator'));
