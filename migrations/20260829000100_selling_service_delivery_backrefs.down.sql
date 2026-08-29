-- Reverse the service-delivery backref columns (drops the mint stamps with them).

ALTER TABLE selling.sales_order_items
    DROP COLUMN IF EXISTS task_id,
    DROP COLUMN IF EXISTS project_id;
