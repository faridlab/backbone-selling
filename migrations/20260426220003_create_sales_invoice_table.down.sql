-- Down: drop selling.sales_invoices table
DROP TABLE IF EXISTS selling.sales_invoices CASCADE;
DROP FUNCTION IF EXISTS selling.sales_invoices_audit_timestamp() CASCADE;
