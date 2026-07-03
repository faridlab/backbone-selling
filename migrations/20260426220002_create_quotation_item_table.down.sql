-- Down: drop selling.quotation_items table
DROP TABLE IF EXISTS selling.quotation_items CASCADE;
DROP FUNCTION IF EXISTS selling.quotation_items_audit_timestamp() CASCADE;
