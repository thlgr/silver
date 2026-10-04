-- Per-bot reasoning effort: the level its turns run at, as the workbench's effort menu picks
-- one. Absent follows the daemon default.
ALTER TABLE bots ADD COLUMN reasoning_effort TEXT;