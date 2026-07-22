-- ADR-0011: fence selling.outbox_events by company_id (extracted from the payload).
ALTER TABLE selling.outbox_events ADD COLUMN IF NOT EXISTS company_id UUID;
UPDATE selling.outbox_events SET company_id = (payload ->> 'company_id')::uuid WHERE company_id IS NULL;
ALTER TABLE selling.outbox_events ALTER COLUMN company_id SET NOT NULL;
CREATE INDEX IF NOT EXISTS idx_selling_outbox_company_id ON selling.outbox_events (company_id);
ALTER TABLE selling.outbox_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE selling.outbox_events FORCE  ROW LEVEL SECURITY;
DROP POLICY IF EXISTS outbox_events_company_isolation ON selling.outbox_events;
CREATE POLICY outbox_events_company_isolation ON selling.outbox_events
    FOR ALL
    USING      (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
