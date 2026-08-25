-- Down: reverse the margin snapshot, the carrier registry + order link, and the
-- expense-reinvoice links (strict reverse of the up migration).

DROP POLICY IF EXISTS expense_reinvoice_links_company_isolation ON selling.expense_reinvoice_links;
DROP TABLE IF EXISTS selling.expense_reinvoice_links CASCADE;
DROP FUNCTION IF EXISTS selling.expense_reinvoice_links_audit_timestamp() CASCADE;

ALTER TABLE selling.sales_order_items
    DROP CONSTRAINT IF EXISTS sales_order_items_unit_cost_non_negative;
ALTER TABLE selling.sales_order_items
    DROP COLUMN IF EXISTS unit_cost;

ALTER TABLE selling.sales_orders
    DROP CONSTRAINT IF EXISTS sales_orders_tracking_ref_max_len;
ALTER TABLE selling.sales_orders
    DROP COLUMN IF EXISTS tracking_ref,
    DROP COLUMN IF EXISTS delivery_carrier_id;

DROP POLICY IF EXISTS delivery_carriers_company_isolation ON selling.delivery_carriers;
DROP TABLE IF EXISTS selling.delivery_carriers CASCADE;
DROP FUNCTION IF EXISTS selling.delivery_carriers_audit_timestamp() CASCADE;

DROP TYPE IF EXISTS expense_reinvoice_state;
