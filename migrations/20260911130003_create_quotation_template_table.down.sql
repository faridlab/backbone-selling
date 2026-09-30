-- Down: drop selling.quotation_templates table
DROP TABLE IF EXISTS selling.quotation_templates CASCADE;
DROP FUNCTION IF EXISTS selling.quotation_templates_audit_timestamp() CASCADE;
