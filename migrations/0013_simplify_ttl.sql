-- Migration 0013: Simplify TTL system to single column
-- Reason: Over-engineered with 5 columns, only need 1
-- Part of: Tightening RCRT to core primitives
-- Date: 2025-11-28

-- Remove over-engineered TTL columns
-- These were added in migration 0007_ttl_enhancements.sql
-- but added unnecessary complexity. We only need the simple ttl column.

ALTER TABLE breadcrumbs DROP COLUMN IF EXISTS ttl_type;
ALTER TABLE breadcrumbs DROP COLUMN IF EXISTS ttl_config;
ALTER TABLE breadcrumbs DROP COLUMN IF EXISTS read_count;
ALTER TABLE breadcrumbs DROP COLUMN IF EXISTS ttl_source;

-- Drop related indexes
DROP INDEX IF EXISTS idx_breadcrumbs_ttl_type;
DROP INDEX IF EXISTS idx_breadcrumbs_read_count;

-- Update comment on ttl column
COMMENT ON COLUMN breadcrumbs.ttl IS 'Expiry timestamp (simple, generic). Use tags like ttl:5min for auto-calculation at creation time.';

-- Keep existing indexes that are still relevant:
-- - idx_breadcrumbs_ttl (for expired breadcrumb cleanup)
-- - idx_breadcrumbs_ttl_expiry (if exists)

-- Benefits:
-- - Simpler schema (1 TTL column instead of 5)
-- - Tag-based TTL rules (ttl:5min, ttl:1hour, etc.)
-- - Less database storage
-- - Aligns with "simple primitives" philosophy
-- - No complex usage-based or hybrid TTL modes

-- Note: TTL rules will now be applied at creation time via tags
-- Example: tags: ["health:check", "ttl:5min"] → auto-sets ttl to NOW() + 5 minutes
-- This is handled in hygiene.rs apply_auto_ttl() function
