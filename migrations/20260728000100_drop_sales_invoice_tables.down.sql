-- Reverse of 20260728000100_drop_sales_invoice_tables.up.sql.
-- Intentionally NOT implemented: restoring selling.sales_invoices + items would require replaying
-- the original create migrations (20260426220003/4) + the IDR-only CHECK (20260426220020) + RLS AND
-- re-adding the schema YAML + regenerating the entity. Selling exited the invoice business (ADR-006);
-- this file exists only to satisfy the migration runner's up/down pair requirement.
SELECT 'down-migration for drop_sales_invoice_tables is intentionally not implemented (ADR-006; selling exited invoices)' AS note;
