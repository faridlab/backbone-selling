DROP POLICY IF EXISTS outbox_events_company_isolation ON selling.outbox_events;
ALTER TABLE selling.outbox_events NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.outbox_events DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS selling.idx_selling_outbox_company_id;
ALTER TABLE selling.outbox_events DROP COLUMN IF EXISTS company_id;
