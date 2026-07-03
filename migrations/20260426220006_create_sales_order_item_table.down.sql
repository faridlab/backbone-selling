-- Down: drop selling.sales_order_items table
DROP TABLE IF EXISTS selling.sales_order_items CASCADE;
DROP FUNCTION IF EXISTS selling.sales_order_items_audit_timestamp() CASCADE;
