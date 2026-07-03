-- Down: drop enum types for selling module
DROP TYPE IF EXISTS sales_order_status CASCADE;
DROP TYPE IF EXISTS gl_posting_state CASCADE;
DROP TYPE IF EXISTS sales_invoice_status CASCADE;
DROP TYPE IF EXISTS quotation_status CASCADE;
