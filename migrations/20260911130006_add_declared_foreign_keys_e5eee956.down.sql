-- Down: drop the declared foreign keys for selling module
ALTER TABLE IF EXISTS selling.sales_person_allocations DROP CONSTRAINT IF EXISTS fk_sales_person_allocations_sales_team_id;
ALTER TABLE IF EXISTS selling.sales_person_allocations DROP CONSTRAINT IF EXISTS fk_sales_person_allocations_order_id;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS fk_sales_order_items_order_id;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS fk_sales_orders_delivery_carrier_id;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS fk_sales_orders_quotation_id;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS fk_quotation_items_quotation_id;
ALTER TABLE IF EXISTS selling.expense_reinvoice_links DROP CONSTRAINT IF EXISTS fk_expense_reinvoice_links_order_id;
