-- Migration 0012: Remove experimental breadcrumb_edges table
-- Reason: GNN (Graph Neural Network) experiment, unused, adds complexity
-- Part of: Tightening RCRT to core primitives
-- Date: 2025-11-28

-- Drop the experimental breadcrumb_edges table
DROP TABLE IF EXISTS breadcrumb_edges CASCADE;

-- Benefits:
-- - Simpler database schema
-- - Faster migrations
-- - Aligns with "everything is a breadcrumb" primitive
-- - Removes unused experimental feature
-- - Reduces database complexity

-- Note: This table was created in migration 0009_breadcrumb_edges.sql
-- as part of a Graph Neural Network context retrieval experiment.
-- The experiment was not adopted, so the table remains unused.
