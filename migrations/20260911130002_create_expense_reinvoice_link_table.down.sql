-- Down: drop selling.expense_reinvoice_links table
DROP TABLE IF EXISTS selling.expense_reinvoice_links CASCADE;
DROP FUNCTION IF EXISTS selling.expense_reinvoice_links_audit_timestamp() CASCADE;
