-- Down: reverse the ADR-0010 Decision A company_id + FORCE RLS fence on the four child tables.
-- Order is the reverse of the up: drop the policy, unfence, drop the column (which cascades
-- to the column-level index), and accept that any rows whose company_id was backfilled will
-- lose that denormalized state (the parent's company_id is unchanged).

-- 4. selling.sales_person_allocations
DROP POLICY IF EXISTS sales_person_allocations_company_isolation ON selling.sales_person_allocations;
ALTER TABLE selling.sales_person_allocations NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_person_allocations DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS idx_sales_person_allocations_company_id;
ALTER TABLE selling.sales_person_allocations DROP COLUMN IF EXISTS company_id;

-- 3. selling.sales_invoice_items
DROP POLICY IF EXISTS sales_invoice_items_company_isolation ON selling.sales_invoice_items;
ALTER TABLE selling.sales_invoice_items NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_invoice_items DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS idx_sales_invoice_items_company_id;
ALTER TABLE selling.sales_invoice_items DROP COLUMN IF EXISTS company_id;

-- 2. selling.sales_order_items
DROP POLICY IF EXISTS sales_order_items_company_isolation ON selling.sales_order_items;
ALTER TABLE selling.sales_order_items NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_order_items DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS idx_sales_order_items_company_id;
ALTER TABLE selling.sales_order_items DROP COLUMN IF EXISTS company_id;

-- 1. selling.quotation_items
DROP POLICY IF EXISTS quotation_items_company_isolation ON selling.quotation_items;
ALTER TABLE selling.quotation_items NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.quotation_items DISABLE ROW LEVEL SECURITY;
DROP INDEX IF EXISTS idx_quotation_items_company_id;
ALTER TABLE selling.quotation_items DROP COLUMN IF EXISTS company_id;
