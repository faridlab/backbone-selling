-- Backbone-outbox 0.2.0 (framework pin v2.7.6, `multi_tenant`) requires `outbox_events.company_id` —
-- `OutboxRecord` carries it and `stage()` writes it as the 5th column. The original outbox table
-- (20260712000100) predates the multi_tenant fence and lacked the column, so `stage()` failed with
-- `column "company_id" of relation "outbox_events" does not exist` once selling bumped to v2.7.6.
--
-- This is additive: bring the column online so `stage()` works again. The RLS fence the crate adds
-- under `multi_tenant` is INTENTIONALLY NOT added here — the outbox is read on unscoped connections
-- (the relay + tests/delivery_outbox_durability.rs); fencing is a separate, coordinated change.

ALTER TABLE selling.outbox_events ADD COLUMN IF NOT EXISTS company_id uuid;

-- Staging has been failing since the v2.7.6 bump, so no live row carries a real company_id; backfill
-- the nil-uuid sentinel to satisfy NOT NULL in any env that still holds pre-fix rows (they are
-- transient — the relay drains them).
UPDATE selling.outbox_events SET company_id = '00000000-0000-0000-0000-000000000000'::uuid
 WHERE company_id IS NULL;

ALTER TABLE selling.outbox_events ALTER COLUMN company_id SET NOT NULL;

CREATE INDEX IF NOT EXISTS idx_selling_outbox_company_id ON selling.outbox_events (company_id);
