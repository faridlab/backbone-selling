-- Invoicing policy + quotation machine fields + the QuotationTemplate master.
-- invoice_policy (order | delivery) is the basis on which a line's quantity becomes
-- invoiceable; is_downpayment marks advance lines that stay on the quantity basis but are
-- excluded from the order invoice-status aggregation. opportunity_id links a quotation to the
-- deal's opportunity (logical FK — the host passes it; no cargo edge). status_reason records why
-- a quotation was rejected/cancelled (set by the machine verbs).

-- Create invoice_policy enum type (guarded: re-run / partially-migrated safe)
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'invoice_policy') THEN
        CREATE TYPE invoice_policy AS ENUM ('order', 'delivery');
    END IF;
END
$$;

-- Quotation lines carry their invoicing policy + downpayment flag; conversion copies both onto
-- the order lines. NOT NULL DEFAULT backfills existing rows in place.
ALTER TABLE selling.quotation_items
    ADD COLUMN invoice_policy invoice_policy NOT NULL DEFAULT 'order',
    ADD COLUMN is_downpayment BOOLEAN NOT NULL DEFAULT false;

-- Order lines: the same pair drives qty_to_invoice, the billed watermark bound, and the status
-- recompute (delivery-policy lines are billed on the delivered basis; downpayment lines stay on
-- the quantity basis and are excluded from the aggregation).
ALTER TABLE selling.sales_order_items
    ADD COLUMN invoice_policy invoice_policy NOT NULL DEFAULT 'order',
    ADD COLUMN is_downpayment BOOLEAN NOT NULL DEFAULT false;

-- Quotation machine fields: the deal link + the reject/cancel reason.
ALTER TABLE selling.quotations
    ADD COLUMN opportunity_id UUID,
    ADD COLUMN status_reason TEXT;

CREATE INDEX IF NOT EXISTS idx_quotations_opportunity_id ON selling.quotations (opportunity_id);

-- The QuotationTemplate master (per-tenant master data; stamps validity + notes on create).
CREATE TABLE IF NOT EXISTS selling.quotation_templates (
    id UUID NOT NULL DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL,
    name TEXT NOT NULL,
    validity_days INTEGER NOT NULL DEFAULT 30,
    default_notes TEXT,
    metadata JSONB NOT NULL DEFAULT '{"created_at":null,"updated_at":null,"deleted_at":null,"created_by":null,"updated_by":null,"deleted_by":null}'::jsonb,
    PRIMARY KEY (id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_quotation_templates_company_id_name
    ON selling.quotation_templates (company_id, name) WHERE (metadata->>'deleted_at') IS NULL;

CREATE INDEX IF NOT EXISTS idx_quotation_templates_company_id ON selling.quotation_templates (company_id);

-- GIN index for audit metadata JSONB queries
CREATE INDEX IF NOT EXISTS idx_quotation_templates_metadata_gin ON selling.quotation_templates USING GIN (metadata);
CREATE INDEX IF NOT EXISTS idx_quotation_templates_metadata_deleted_at ON selling.quotation_templates ((metadata->>'deleted_at'));
CREATE INDEX IF NOT EXISTS idx_quotation_templates_metadata_created_at ON selling.quotation_templates ((metadata->>'created_at'));
CREATE INDEX IF NOT EXISTS idx_quotation_templates_metadata_updated_at ON selling.quotation_templates ((metadata->>'updated_at'));

-- Triggers for automatic metadata timestamp management
CREATE OR REPLACE FUNCTION selling.quotation_templates_audit_timestamp() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{created_at}', to_jsonb(NOW()));
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{updated_at}', to_jsonb(NOW()));
    ELSIF TG_OP = 'UPDATE' THEN
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{updated_at}', to_jsonb(NOW()));
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS quotation_templates_insert_audit ON selling.quotation_templates;
CREATE TRIGGER quotation_templates_insert_audit BEFORE INSERT ON selling.quotation_templates
    FOR EACH ROW EXECUTE FUNCTION selling.quotation_templates_audit_timestamp();

DROP TRIGGER IF EXISTS quotation_templates_update_audit ON selling.quotation_templates;
CREATE TRIGGER quotation_templates_update_audit BEFORE UPDATE ON selling.quotation_templates
    FOR EACH ROW EXECUTE FUNCTION selling.quotation_templates_audit_timestamp();

-- Company RLS fence for selling.quotation_templates (ADR-0008). company_id is scoped per request
-- via `set_config('app.company_id', <uuid>, true)`; an unset var sees zero rows.
ALTER TABLE selling.quotation_templates ENABLE ROW LEVEL SECURITY;
ALTER TABLE selling.quotation_templates FORCE  ROW LEVEL SECURITY;
DROP POLICY IF EXISTS quotation_templates_company_isolation ON selling.quotation_templates;
CREATE POLICY quotation_templates_company_isolation ON selling.quotation_templates
    FOR ALL
    USING      (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
