-- Down: drop selling.quotations table
DROP TABLE IF EXISTS selling.quotations CASCADE;
DROP FUNCTION IF EXISTS selling.quotations_audit_timestamp() CASCADE;
