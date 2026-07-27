-- Down: drop enum types for selling module
DROP TYPE IF EXISTS sales_order_status CASCADE;
DROP TYPE IF EXISTS quotation_status CASCADE;
