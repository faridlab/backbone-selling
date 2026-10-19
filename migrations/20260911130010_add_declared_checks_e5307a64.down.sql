-- Down: drop the declared rule constraints for selling module
ALTER TABLE IF EXISTS selling.sales_person_allocations DROP CONSTRAINT IF EXISTS ck_sales_person_allocations_allocated_pc_6e67b5e1__non_negative;
ALTER TABLE IF EXISTS selling.sales_person_allocations DROP CONSTRAINT IF EXISTS ck_sales_person_allocations_sales_person_id_required__required;
ALTER TABLE IF EXISTS selling.sales_person_allocations DROP CONSTRAINT IF EXISTS ck_sales_person_allocations_order_id_required__required;
ALTER TABLE IF EXISTS selling.sales_teams DROP CONSTRAINT IF EXISTS ck_sales_teams_description_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_teams DROP CONSTRAINT IF EXISTS ck_sales_teams_name_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_teams DROP CONSTRAINT IF EXISTS ck_sales_teams_name_required__required;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_delivered_qty_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_billed_qty_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_unit_cost_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_line_amount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_line_discount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_unit_price_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_quantity_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_description_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_item_id_required__required;
ALTER TABLE IF EXISTS selling.sales_order_items DROP CONSTRAINT IF EXISTS ck_sales_order_items_order_id_required__required;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_notes_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_total_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_tax_amount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_tax_rate_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_subtotal_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_currency_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_customer_id_required__required;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_tracking_ref_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_order_number_max_length__max_length;
ALTER TABLE IF EXISTS selling.sales_orders DROP CONSTRAINT IF EXISTS ck_sales_orders_order_number_required__required;
DO $$
BEGIN
    IF to_regclass('selling.uq_sales_orders_order_number__unique') IS NOT NULL AND to_regclass('selling.idx_sales_orders_order_number') IS NULL THEN
        ALTER INDEX selling.uq_sales_orders_order_number__unique RENAME TO idx_sales_orders_order_number;
    END IF;
END $$;

ALTER TABLE IF EXISTS selling.quotation_templates DROP CONSTRAINT IF EXISTS ck_quotation_templates_default_notes_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotation_templates DROP CONSTRAINT IF EXISTS ck_quotation_templates_name_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotation_templates DROP CONSTRAINT IF EXISTS ck_quotation_templates_name_required__required;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_line_amount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_line_discount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_unit_price_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_quantity_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_description_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_item_id_required__required;
ALTER TABLE IF EXISTS selling.quotation_items DROP CONSTRAINT IF EXISTS ck_quotation_items_quotation_id_required__required;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_status_reason_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_notes_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_total_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_tax_amount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_tax_rate_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_subtotal_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_currency_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_customer_id_required__required;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_quotation_number_max_length__max_length;
ALTER TABLE IF EXISTS selling.quotations DROP CONSTRAINT IF EXISTS ck_quotations_quotation_number_required__required;
DO $$
BEGIN
    IF to_regclass('selling.uq_quotations_quotation_number__unique') IS NOT NULL AND to_regclass('selling.idx_quotations_quotation_number') IS NULL THEN
        ALTER INDEX selling.uq_quotations_quotation_number__unique RENAME TO idx_quotations_quotation_number;
    END IF;
END $$;

ALTER TABLE IF EXISTS selling.expense_reinvoice_links DROP CONSTRAINT IF EXISTS ck_expense_reinvoice_links_amount_non_negative__non_negative;
ALTER TABLE IF EXISTS selling.expense_reinvoice_links DROP CONSTRAINT IF EXISTS ck_expense_reinvoice_links_expense_id_required__required;
ALTER TABLE IF EXISTS selling.expense_reinvoice_links DROP CONSTRAINT IF EXISTS ck_expense_reinvoice_links_order_id_required__required;
DO $$
BEGIN
    IF to_regclass('selling.uq_expense_reinvoice_links_expense_id__unique') IS NOT NULL AND to_regclass('selling.idx_expense_reinvoice_links_order_id_expense_id') IS NULL THEN
        ALTER INDEX selling.uq_expense_reinvoice_links_expense_id__unique RENAME TO idx_expense_reinvoice_links_order_id_expense_id;
    END IF;
END $$;

ALTER TABLE IF EXISTS selling.delivery_carriers DROP CONSTRAINT IF EXISTS ck_delivery_carriers_tracking_url_template_506e0cc0__max_length;
ALTER TABLE IF EXISTS selling.delivery_carriers DROP CONSTRAINT IF EXISTS ck_delivery_carriers_name_max_length__max_length;
ALTER TABLE IF EXISTS selling.delivery_carriers DROP CONSTRAINT IF EXISTS ck_delivery_carriers_name_required__required;
