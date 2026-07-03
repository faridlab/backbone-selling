-- Down: drop selling.sales_invoice_items table
DROP TABLE IF EXISTS selling.sales_invoice_items CASCADE;
DROP FUNCTION IF EXISTS selling.sales_invoice_items_audit_timestamp() CASCADE;
