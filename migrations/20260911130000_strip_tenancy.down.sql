-- Hand-authored (user-owned). Not regenerated.
--
-- Best-effort restore sketch for the tenancy strip (ADR-0029). This is a breaking module
-- release against dev-stage databases: the down re-adds the company_id column as nullable
-- with its plain index and the company isolation policy shape, but restores NO data —
-- rows written after the strip (or after the decorator re-keyed them) carry org_unit_id
-- only. The composing service's tenancy decorator remains the live fence; treat this
-- down as a schema-shape sketch for archaeology, not a usable rollback.
--
-- selling.sales_invoice_items (the pre-ADR-006 residue) is not restored at all: the
-- table itself is no longer part of this module's chain.

ALTER TABLE selling.quotations             ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.quotation_items        ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.sales_orders           ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.sales_order_items      ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.sales_teams            ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.sales_person_allocations ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.quotation_templates    ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.delivery_carriers      ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE selling.expense_reinvoice_links ADD COLUMN IF NOT EXISTS company_id uuid;

CREATE INDEX IF NOT EXISTS idx_quotations_company_id_customer_id_status
    ON selling.quotations (company_id, customer_id, status);
CREATE INDEX IF NOT EXISTS idx_quotation_items_company_id
    ON selling.quotation_items (company_id);
CREATE INDEX IF NOT EXISTS idx_sales_orders_company_id_customer_id_status
    ON selling.sales_orders (company_id, customer_id, status);
CREATE INDEX IF NOT EXISTS idx_sales_order_items_company_id
    ON selling.sales_order_items (company_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sales_teams_company_id_name
    ON selling.sales_teams (company_id, name);
CREATE INDEX IF NOT EXISTS idx_sales_person_allocations_company_id
    ON selling.sales_person_allocations (company_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_quotation_templates_company_id_name
    ON selling.quotation_templates (company_id, name);
CREATE INDEX IF NOT EXISTS idx_quotation_templates_company_id
    ON selling.quotation_templates (company_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_carriers_company_id_name
    ON selling.delivery_carriers (company_id, name);
CREATE INDEX IF NOT EXISTS idx_delivery_carriers_company_id
    ON selling.delivery_carriers (company_id);
CREATE INDEX IF NOT EXISTS idx_expense_reinvoice_links_company_id_state
    ON selling.expense_reinvoice_links (company_id, state);

CREATE POLICY quotations_company_isolation ON selling.quotations
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY quotation_items_company_isolation ON selling.quotation_items
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY sales_orders_company_isolation ON selling.sales_orders
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY sales_order_items_company_isolation ON selling.sales_order_items
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY sales_teams_company_isolation ON selling.sales_teams
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY sales_person_allocations_company_isolation ON selling.sales_person_allocations
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY quotation_templates_company_isolation ON selling.quotation_templates
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY delivery_carriers_company_isolation ON selling.delivery_carriers
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY expense_reinvoice_links_company_isolation ON selling.expense_reinvoice_links
    FOR ALL USING (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
