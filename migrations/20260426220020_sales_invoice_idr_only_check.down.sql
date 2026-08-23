-- Conditional for the same reason as the up side: on a fresh database selling.sales_invoices never
-- exists (its CREATE migrations left the chain with the invoice-business exit, ADR-006), so there
-- is no table to drop the constraint from.
DO $$
BEGIN
    IF to_regclass('selling.sales_invoices') IS NOT NULL THEN
        ALTER TABLE selling.sales_invoices DROP CONSTRAINT IF EXISTS sales_invoices_idr_only;
    END IF;
END
$$;
