ALTER TABLE workspace_metadata ADD COLUMN synced_at DATETIME;
ALTER TABLE workspace_metadata ADD COLUMN index_bytes BIGINT;
ALTER TABLE workspace_metadata ADD COLUMN file_count INTEGER;
ALTER TABLE workspace_metadata ADD COLUMN fragment_count INTEGER;
ALTER TABLE workspace_metadata ADD COLUMN query_count INTEGER NOT NULL DEFAULT 0;

-- Backfill query_count for workspaces that already had query history before
-- the column existed. Without this, the settings UI would render "queried 0
-- times" for every legacy workspace until the next retrieval bumps it.
UPDATE workspace_metadata SET query_count = 1 WHERE queried_ts IS NOT NULL;
