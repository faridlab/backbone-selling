-- Down: drop selling.delivery_carriers table
DROP TABLE IF EXISTS selling.delivery_carriers CASCADE;
DROP FUNCTION IF EXISTS selling.delivery_carriers_audit_timestamp() CASCADE;
