-- Down: drop selling.sales_teams table
DROP TABLE IF EXISTS selling.sales_teams CASCADE;
DROP FUNCTION IF EXISTS selling.sales_teams_audit_timestamp() CASCADE;
