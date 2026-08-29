-- Service-delivery backrefs on the sales-order lines.
--
-- A product can carry a service-tracking policy (a task in a fixed project, a task in a
-- per-order project, just the per-order project, or manual = nothing). When a sales order
-- carrying such a product is CONFIRMED, selling asks the project side (through the
-- ProjectFulfillmentPort) to mint the delivery work, and stamps what was minted back onto
-- the order line here: the project the line's delivery lives in, and (when the policy
-- mints tasks) the task. These are logical references — project.Project.id and
-- project.Task.id — with no cross-module foreign key by design; the composing host owns
-- both sides of the seam.
--
-- NULL = this line minted nothing (a manual policy, a product the composition does not
-- track, or an order confirmed before this seam existed). Not backfilled: pre-existing
-- confirmed orders keep NULL forever — honest absence.

ALTER TABLE selling.sales_order_items
    ADD COLUMN IF NOT EXISTS project_id UUID,
    ADD COLUMN IF NOT EXISTS task_id UUID;
