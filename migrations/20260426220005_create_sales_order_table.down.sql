-- Down: drop selling.sales_orders table
DROP TABLE IF EXISTS selling.sales_orders CASCADE;
DROP FUNCTION IF EXISTS selling.sales_orders_audit_timestamp() CASCADE;
