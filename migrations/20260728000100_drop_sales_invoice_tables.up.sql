-- ADR-006: selling exits the invoice business. The composed flow has used backbone-billing's real
-- SalesInvoice since ADR-005; selling's own tables were dead-in-composed-flow. Drop them.
-- Data-loss: NONE in the composed flow (the tables were empty — selling's invoice write path was
-- already retired); any standalone local-dev rows will be lost.
DROP TABLE IF EXISTS selling.sales_invoice_items CASCADE;
DROP TABLE IF EXISTS selling.sales_invoices CASCADE;
-- CHECK constraint (IDR-only), RLS policies, audit trigger, indexes all CASCADE with the tables.
-- Enum types `sales_invoice_status` + `gl_posting_state` are left in place (shared/global, harmless).
