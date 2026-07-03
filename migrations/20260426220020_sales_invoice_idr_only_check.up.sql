-- Hand-authored constraint (council 2026-07-03, ADR-002): revenue posting keeps the GL in the
-- company base currency (IDR) and the AccountingPostEnvelope carries no exchange_rate. Until a
-- multi-currency FX contract is designed, a sales invoice must be IDR — so a non-IDR invoice can
-- never be created and then post foreign face-value amounts into an IDR ledger. Belt-and-suspenders
-- with the `build_revenue_post` guard (unsupported_currency, 422).
ALTER TABLE selling.sales_invoices
    ADD CONSTRAINT sales_invoices_idr_only CHECK (currency = 'IDR');
