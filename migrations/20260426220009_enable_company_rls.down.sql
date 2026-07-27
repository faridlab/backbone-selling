-- Down: remove the company RLS fence for selling module

-- Reverse the company RLS fence for selling.quotations
DROP POLICY IF EXISTS quotations_company_isolation ON selling.quotations;
ALTER TABLE selling.quotations NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.quotations DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.quotation_items
DROP POLICY IF EXISTS quotation_items_company_isolation ON selling.quotation_items;
ALTER TABLE selling.quotation_items NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.quotation_items DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.sales_invoices
DROP POLICY IF EXISTS sales_invoices_company_isolation ON selling.sales_invoices;
ALTER TABLE selling.sales_invoices NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_invoices DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.sales_invoice_items
DROP POLICY IF EXISTS sales_invoice_items_company_isolation ON selling.sales_invoice_items;
ALTER TABLE selling.sales_invoice_items NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_invoice_items DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.sales_orders
DROP POLICY IF EXISTS sales_orders_company_isolation ON selling.sales_orders;
ALTER TABLE selling.sales_orders NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_orders DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.sales_order_items
DROP POLICY IF EXISTS sales_order_items_company_isolation ON selling.sales_order_items;
ALTER TABLE selling.sales_order_items NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_order_items DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.sales_teams
DROP POLICY IF EXISTS sales_teams_company_isolation ON selling.sales_teams;
ALTER TABLE selling.sales_teams NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_teams DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for selling.sales_person_allocations
DROP POLICY IF EXISTS sales_person_allocations_company_isolation ON selling.sales_person_allocations;
ALTER TABLE selling.sales_person_allocations NO FORCE ROW LEVEL SECURITY;
ALTER TABLE selling.sales_person_allocations DISABLE ROW LEVEL SECURITY;

