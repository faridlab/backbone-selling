-- Down: drop the quotation template master, the quotation machine fields, and the invoicing
-- policy columns + type (reverse of the up migration, in reverse order).

DROP POLICY IF EXISTS quotation_templates_company_isolation ON selling.quotation_templates;
DROP TABLE IF EXISTS selling.quotation_templates CASCADE;
DROP FUNCTION IF EXISTS selling.quotation_templates_audit_timestamp() CASCADE;

DROP INDEX IF EXISTS selling.idx_quotations_opportunity_id;
ALTER TABLE selling.quotations
    DROP COLUMN IF EXISTS status_reason,
    DROP COLUMN IF EXISTS opportunity_id;

ALTER TABLE selling.sales_order_items
    DROP COLUMN IF EXISTS is_downpayment,
    DROP COLUMN IF EXISTS invoice_policy;

ALTER TABLE selling.quotation_items
    DROP COLUMN IF EXISTS is_downpayment,
    DROP COLUMN IF EXISTS invoice_policy;

DROP TYPE IF EXISTS invoice_policy;
