-- Unit-cost margin snapshot + delivery-carrier registry + expense-reinvoice links.
--
-- sales_order_items.unit_cost is the CONFIRM-TIME cost snapshot (written only by the confirm
-- stamp through the UnitCostPort; NULL = no cost maintained — margin reads NULL, never zero).
-- Existing confirmed orders are NOT backfilled: they read margin NULL forever (honest absence).
-- sales_orders.delivery_carrier_id + tracking_ref are fulfillment metadata (intra-module FK to
-- the new delivery_carriers master; writable on draft AND confirmed orders, refused on cancelled).
-- expense_reinvoice_links attaches a customer-rebillable employee expense to an order (logical
-- ref to expenses.Expense — no cross-module key; the host billing adapter pulls pending links
-- and marks them invoiced).

-- Expense-reinvoice link state (guarded: re-run / partially-migrated safe)
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'expense_reinvoice_state') THEN
        CREATE TYPE expense_reinvoice_state AS ENUM ('pending', 'invoiced');
    END IF;
END
$$;

-- The DeliveryCarrier master (per-tenant registry only — no rates, no labels, no carrier API).
CREATE TABLE IF NOT EXISTS selling.delivery_carriers (
    id UUID NOT NULL DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL,
    name TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    tracking_url_template TEXT,
    metadata JSONB NOT NULL DEFAULT '{"created_at":null,"updated_at":null,"deleted_at":null,"created_by":null,"updated_by":null,"deleted_by":null}'::jsonb,
    PRIMARY KEY (id)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_carriers_company_id_name
    ON selling.delivery_carriers (company_id, name) WHERE (metadata->>'deleted_at') IS NULL;

CREATE INDEX IF NOT EXISTS idx_delivery_carriers_company_id ON selling.delivery_carriers (company_id);

-- GIN index for audit metadata JSONB queries
CREATE INDEX IF NOT EXISTS idx_delivery_carriers_metadata_gin ON selling.delivery_carriers USING GIN (metadata);
CREATE INDEX IF NOT EXISTS idx_delivery_carriers_metadata_deleted_at ON selling.delivery_carriers ((metadata->>'deleted_at'));
CREATE INDEX IF NOT EXISTS idx_delivery_carriers_metadata_created_at ON selling.delivery_carriers ((metadata->>'created_at'));
CREATE INDEX IF NOT EXISTS idx_delivery_carriers_metadata_updated_at ON selling.delivery_carriers ((metadata->>'updated_at'));

-- Triggers for automatic metadata timestamp management
CREATE OR REPLACE FUNCTION selling.delivery_carriers_audit_timestamp() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{created_at}', to_jsonb(NOW()));
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{updated_at}', to_jsonb(NOW()));
    ELSIF TG_OP = 'UPDATE' THEN
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{updated_at}', to_jsonb(NOW()));
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS delivery_carriers_insert_audit ON selling.delivery_carriers;
CREATE TRIGGER delivery_carriers_insert_audit BEFORE INSERT ON selling.delivery_carriers
    FOR EACH ROW EXECUTE FUNCTION selling.delivery_carriers_audit_timestamp();

DROP TRIGGER IF EXISTS delivery_carriers_update_audit ON selling.delivery_carriers;
CREATE TRIGGER delivery_carriers_update_audit BEFORE UPDATE ON selling.delivery_carriers
    FOR EACH ROW EXECUTE FUNCTION selling.delivery_carriers_audit_timestamp();

-- Company RLS fence for selling.delivery_carriers (ADR-0008).
ALTER TABLE selling.delivery_carriers ENABLE ROW LEVEL SECURITY;
ALTER TABLE selling.delivery_carriers FORCE  ROW LEVEL SECURITY;
DROP POLICY IF EXISTS delivery_carriers_company_isolation ON selling.delivery_carriers;
CREATE POLICY delivery_carriers_company_isolation ON selling.delivery_carriers
    FOR ALL
    USING      (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);

-- Order fulfillment metadata: the carrier choice + the tracking number. NOT a frozen money
-- field — writable after confirm (tracking typically arrives after ship), refused on cancelled.
ALTER TABLE selling.sales_orders
    ADD COLUMN IF NOT EXISTS delivery_carrier_id UUID REFERENCES selling.delivery_carriers(id),
    ADD COLUMN IF NOT EXISTS tracking_ref TEXT;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'sales_orders_tracking_ref_max_len'
    ) THEN
        ALTER TABLE selling.sales_orders
            ADD CONSTRAINT sales_orders_tracking_ref_max_len CHECK (char_length(tracking_ref) <= 120) NOT VALID;
    END IF;
END
$$;
ALTER TABLE selling.sales_orders VALIDATE CONSTRAINT sales_orders_tracking_ref_max_len;

-- The confirm-time unit-cost snapshot. Nullable, NOT backfilled (pre-existing confirmed orders
-- read margin NULL forever — honest absence), non-negative. Online-safe: NOT VALID + VALIDATE.
ALTER TABLE selling.sales_order_items
    ADD COLUMN IF NOT EXISTS unit_cost NUMERIC(18,6);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'sales_order_items_unit_cost_non_negative'
    ) THEN
        ALTER TABLE selling.sales_order_items
            ADD CONSTRAINT sales_order_items_unit_cost_non_negative CHECK (unit_cost >= 0) NOT VALID;
    END IF;
END
$$;
ALTER TABLE selling.sales_order_items VALIDATE CONSTRAINT sales_order_items_unit_cost_non_negative;

-- The expense-reinvoice association: one rebillable charge of a given expense per order (the
-- partial unique index is the double-bill guard; a soft-deleted link can be re-attached).
CREATE TABLE IF NOT EXISTS selling.expense_reinvoice_links (
    id UUID NOT NULL DEFAULT gen_random_uuid(),
    company_id UUID NOT NULL,
    order_id UUID NOT NULL REFERENCES selling.sales_orders(id),
    expense_id UUID NOT NULL,
    amount NUMERIC(18,2) NOT NULL,
    state expense_reinvoice_state NOT NULL DEFAULT 'pending',
    metadata JSONB NOT NULL DEFAULT '{"created_at":null,"updated_at":null,"deleted_at":null,"created_by":null,"updated_by":null,"deleted_by":null}'::jsonb,
    PRIMARY KEY (id),
    CONSTRAINT expense_reinvoice_links_amount_non_negative CHECK (amount >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_expense_reinvoice_links_order_id_expense_id
    ON selling.expense_reinvoice_links (order_id, expense_id) WHERE (metadata->>'deleted_at') IS NULL;

CREATE INDEX IF NOT EXISTS idx_expense_reinvoice_links_company_id_state
    ON selling.expense_reinvoice_links (company_id, state);

CREATE INDEX IF NOT EXISTS idx_expense_reinvoice_links_metadata_gin ON selling.expense_reinvoice_links USING GIN (metadata);
CREATE INDEX IF NOT EXISTS idx_expense_reinvoice_links_metadata_deleted_at ON selling.expense_reinvoice_links ((metadata->>'deleted_at'));
CREATE INDEX IF NOT EXISTS idx_expense_reinvoice_links_metadata_created_at ON selling.expense_reinvoice_links ((metadata->>'created_at'));
CREATE INDEX IF NOT EXISTS idx_expense_reinvoice_links_metadata_updated_at ON selling.expense_reinvoice_links ((metadata->>'updated_at'));

-- Triggers for automatic metadata timestamp management
CREATE OR REPLACE FUNCTION selling.expense_reinvoice_links_audit_timestamp() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{created_at}', to_jsonb(NOW()));
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{updated_at}', to_jsonb(NOW()));
    ELSIF TG_OP = 'UPDATE' THEN
        NEW.metadata = jsonb_set(NEW.metadata::jsonb, '{updated_at}', to_jsonb(NOW()));
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS expense_reinvoice_links_insert_audit ON selling.expense_reinvoice_links;
CREATE TRIGGER expense_reinvoice_links_insert_audit BEFORE INSERT ON selling.expense_reinvoice_links
    FOR EACH ROW EXECUTE FUNCTION selling.expense_reinvoice_links_audit_timestamp();

DROP TRIGGER IF EXISTS expense_reinvoice_links_update_audit ON selling.expense_reinvoice_links;
CREATE TRIGGER expense_reinvoice_links_update_audit BEFORE UPDATE ON selling.expense_reinvoice_links
    FOR EACH ROW EXECUTE FUNCTION selling.expense_reinvoice_links_audit_timestamp();

-- Company RLS fence for selling.expense_reinvoice_links (ADR-0008).
ALTER TABLE selling.expense_reinvoice_links ENABLE ROW LEVEL SECURITY;
ALTER TABLE selling.expense_reinvoice_links FORCE  ROW LEVEL SECURITY;
DROP POLICY IF EXISTS expense_reinvoice_links_company_isolation ON selling.expense_reinvoice_links;
CREATE POLICY expense_reinvoice_links_company_isolation ON selling.expense_reinvoice_links
    FOR ALL
    USING      (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
