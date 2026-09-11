-- Hand-authored (user-owned). Not regenerated.
--
-- Strip every company-fence artifact from the selling tables (ADR-0029): the module is
-- tenant-agnostic; org scoping is installed by the COMPOSING service's tenancy decorator,
-- never by the module. Dropped here, per table: the company-leading indexes and uniques,
-- the <table>_company_isolation RLS policy, and the company_id column itself.
--
-- Ordering guard (the decorator must run FIRST on any database with data): the module
-- never moves tenancy data. A table is safe to strip when EITHER
--   a) it carries org_unit_id with no NULLs — the decorator backfilled it from company_id —
--      or b) it is empty (a fresh database: the earlier chain files created it empty).
-- Otherwise the strip RAISEs, naming the decorator step, rather than dropping a column
-- that still holds the only tenancy key. The file is re-runnable (every drop is IF EXISTS
-- and the tracker has no checksums), so a failed run retries cleanly after the decorator
-- lands.
--
-- RLS enable/force flags are deliberately NOT touched: the decorator owns those now.
-- The same goes for selling.outbox_events.company_id: the multi-tenant outbox crate's
-- OutboxRecord still requires the column, so it stays and the module fills it with the
-- ambient org scope's legacy company echo.
--
-- Residue note: databases provisioned before selling exited the invoice business may
-- still carry selling.sales_invoice_items with its company fence; its artifacts are
-- dropped below under a to_regclass guard, exactly once, best-effort.

DO $$
DECLARE
    t text;
    has_org boolean;
    org_nulls bigint;
    total bigint;
    offenders text := '';
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'quotations', 'quotation_items',
        'sales_orders', 'sales_order_items',
        'sales_teams', 'sales_person_allocations',
        'quotation_templates', 'delivery_carriers',
        'expense_reinvoice_links'
    ]
    LOOP
        IF to_regclass(format('selling.%I', t)) IS NULL THEN
            CONTINUE; -- chain not fully applied on this database; nothing to strip
        END IF;

        SELECT EXISTS (
                   SELECT 1 FROM information_schema.columns
                   WHERE table_schema = 'selling' AND table_name = t AND column_name = 'org_unit_id'
               )
        INTO has_org;

        EXECUTE format('SELECT count(*) FROM selling.%I', t) INTO total;

        IF has_org THEN
            EXECUTE format(
                'SELECT count(*) FROM selling.%I WHERE org_unit_id IS NULL', t)
            INTO org_nulls;
        ELSE
            org_nulls := total; -- no org column: every row's only tenancy key is company_id
        END IF;

        IF has_org AND org_nulls = 0 THEN
            CONTINUE; -- decorator backfilled: safe
        END IF;
        IF total = 0 THEN
            CONTINUE; -- empty table (fresh database): safe
        END IF;
        offenders := offenders || format(' selling.%s (%s rows, %s rows not covered by org_unit_id);', t, total, org_nulls);
    END LOOP;

    IF offenders <> '' THEN
        RAISE EXCEPTION 'refusing to strip company_id — these tables are not yet covered by the tenancy decorator:%. Apply the composing service''s tenancy decorator (it backfills org_unit_id from company_id) and re-run; it is the only step that moves tenancy data.', offenders;
    END IF;
END $$;

-- ── quotations ─────────────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_quotations_company_id_customer_id_status;
DROP POLICY IF EXISTS quotations_company_isolation ON selling.quotations;
ALTER TABLE selling.quotations DROP COLUMN IF EXISTS company_id;

-- ── quotation_items ────────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_quotation_items_company_id;
DROP POLICY IF EXISTS quotation_items_company_isolation ON selling.quotation_items;
ALTER TABLE selling.quotation_items DROP COLUMN IF EXISTS company_id;

-- ── sales_orders ───────────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_sales_orders_company_id_customer_id_status;
DROP POLICY IF EXISTS sales_orders_company_isolation ON selling.sales_orders;
ALTER TABLE selling.sales_orders DROP COLUMN IF EXISTS company_id;

-- ── sales_order_items ──────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_sales_order_items_company_id;
DROP POLICY IF EXISTS sales_order_items_company_isolation ON selling.sales_order_items;
ALTER TABLE selling.sales_order_items DROP COLUMN IF EXISTS company_id;

-- ── sales_teams ────────────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_sales_teams_company_id_name;
DROP POLICY IF EXISTS sales_teams_company_isolation ON selling.sales_teams;
ALTER TABLE selling.sales_teams DROP COLUMN IF EXISTS company_id;

-- ── sales_person_allocations ───────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_sales_person_allocations_company_id;
DROP POLICY IF EXISTS sales_person_allocations_company_isolation ON selling.sales_person_allocations;
ALTER TABLE selling.sales_person_allocations DROP COLUMN IF EXISTS company_id;

-- ── quotation_templates ────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_quotation_templates_company_id_name;
DROP INDEX IF EXISTS selling.idx_quotation_templates_company_id;
DROP POLICY IF EXISTS quotation_templates_company_isolation ON selling.quotation_templates;
ALTER TABLE selling.quotation_templates DROP COLUMN IF EXISTS company_id;

-- ── delivery_carriers ──────────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_delivery_carriers_company_id_name;
DROP INDEX IF EXISTS selling.idx_delivery_carriers_company_id;
DROP POLICY IF EXISTS delivery_carriers_company_isolation ON selling.delivery_carriers;
ALTER TABLE selling.delivery_carriers DROP COLUMN IF EXISTS company_id;

-- ── expense_reinvoice_links ────────────────────────────────────────────────────
DROP INDEX IF EXISTS selling.idx_expense_reinvoice_links_company_id_state;
DROP POLICY IF EXISTS expense_reinvoice_links_company_isolation ON selling.expense_reinvoice_links;
ALTER TABLE selling.expense_reinvoice_links DROP COLUMN IF EXISTS company_id;

-- ── legacy residue: sales_invoice_items (pre-ADR-006 databases only) ───────────
DO $$
BEGIN
    IF to_regclass('selling.sales_invoice_items') IS NOT NULL THEN
        DROP INDEX IF EXISTS selling.idx_sales_invoice_items_company_id;
        DROP POLICY IF EXISTS sales_invoice_items_company_isolation ON selling.sales_invoice_items;
        ALTER TABLE selling.sales_invoice_items DROP COLUMN IF EXISTS company_id;
    END IF;
END
$$;
