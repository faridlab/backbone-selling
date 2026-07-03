-- Down: drop selling.sales_person_allocations table
DROP TABLE IF EXISTS selling.sales_person_allocations CASCADE;
DROP FUNCTION IF EXISTS selling.sales_person_allocations_audit_timestamp() CASCADE;
