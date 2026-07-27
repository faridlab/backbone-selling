-- Reverse of 20260727000100_outbox_company_id.up.sql.
DROP INDEX IF EXISTS selling.idx_selling_outbox_company_id;
ALTER TABLE selling.outbox_events ALTER COLUMN company_id DROP NOT NULL;
ALTER TABLE selling.outbox_events DROP COLUMN IF EXISTS company_id;
