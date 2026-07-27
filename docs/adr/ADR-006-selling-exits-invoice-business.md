# ADR-006: Selling exits the invoice business — billing owns AR invoicing + revenue, end to end

**Status**: Accepted — **Applied 2026-07-27**
**Deciders**: Farid (owner), council-#3 migration session 2026-07-27
**Supersedes**: [ADR-001](ADR-001-selling-boundary.md) (the folded-in revenue-recognition half), [ADR-002](ADR-002-gl-posting-seam.md) (selling's GL-posting seam)
**Related**: [ADR-005](ADR-005-invoice-seam.md) (the billing-owned invoice seam that made this possible), the 2026-07-27 [maturity council](../council/2026-07-27-module-backbone-selling-maturity.md) (#3)

## Context

ADR-001 *temporarily* folded revenue recognition (`SalesInvoice` + the revenue `AccountingPostEnvelope`) into selling so the marquee GL-posting seam (ADR-002) could be proven before a separate billing module existed. ADR-005 then built the real seam: selling emits an `InvoiceRequestEnvelope`, **billing** raises + posts the invoice (revenue journal), and selling's `mark_invoiced` advances `billed_qty` from billing's `SalesInvoicePosted`. After ADR-005, selling's own invoice write/post path (`create_sales_invoice`, `create_invoice_from_order`, `post_sales_invoice`, `build_revenue_post`, `selling_gl`) was **dead-in-composed-flow** — and its continued presence created the "Invoice double-meaning" the 2026-07-27 council's DDD seat flagged.

## Decision

Selling **fully exits the invoice business**. Billing owns the AR invoice + revenue recognition, end to end.

1. **Remove the schema entity.** Delete `schema/models/sales_invoice.model.yaml` and regenerate (`metaphor schema generate --force`): the generated `SalesInvoice` surface (entity, services, handlers, DTOs, routes, seeders, validators, etc.) is no longer emitted; `lib.rs`'s `SellingModule` no longer holds invoice services or routes.
2. **Delete the user-owned write/post path.** Remove `selling_invoice_create.rs`, `selling_invoice_post.rs`, `selling_gl.rs`, and the two `sales_invoice_*_repository.rs` files.
3. **Drop the GL seam from selling.** Remove the `backbone-gl-posting` Cargo dependency and the `SellingEvent::SalesInvoiceIssued` / `SalesInvoicePosted` events. Selling posts nothing to the GL. The seam *pattern* lives on in `backbone-billing`.
4. **Relocate the shared helper.** `recompute_order_status` (the watermark → status rollup, ADR-003) moved from the deleted `selling_invoice_post.rs` into `selling_write_service.rs` — it is still shared by `mark_delivered` + `mark_invoiced`.
5. **Keep the billing-owned seam.** `build_invoice_request` (outbound `InvoiceRequestEnvelope`) and `mark_invoiced` (inbound, advances `billed_qty`) survive unchanged — they are selling's entire invoice surface now.
6. **Drop the tables.** Migration `20260728000100_drop_sales_invoice_tables` drops `selling.sales_invoices` + `sales_invoice_items` (CASCADE). The `sales_invoice_status` + `gl_posting_state` enums are left in place (shared/global, harmless). No data loss in the composed flow (the tables were empty after ADR-005).
7. **Relocate the coverage.** The 7 invoice golden cases + 4 GL-seam proofs move to `backbone-billing`: `billing/tests/billing_golden_cases.rs` gains the revenue-grouped case; `billing/tests/ar_seam.rs` (new) proves the balanced real-ledger AR revenue post + concurrent-double-post. Selling keeps `tests/invoice_seam.rs` (the billing-owned flow) and `tests/selling_golden_cases.rs::quotation_order_confirm_flow` (SGC-7).
8. **Trim the delivery e2e.** `tests/delivery_seam.rs` now stops at `to_bill` (delivered, awaiting billing); the revenue leg + `completed` are `invoice_seam.rs` / billing's job.

## Consequences

- Selling is **no longer a GL producer** (the ADR-001 framing is retired). Its outward surface is: order-to-cash *intent* (Quotation → Sales Order), the delivery seam (→ inventory), and the invoice *request/handle* seam (→ billing). Cross-module refs stay logical FKs; `cargo tree -e normal -i backbone-gl-posting` from selling is empty.
- The "Invoice double-meaning" DDD boundary violation is resolved: there is exactly one SalesInvoice concept now, and it lives in billing.
- AR revenue-post coverage (balanced journal, idempotency, concurrency, IDR guard) is preserved in `backbone-billing` — not lost.
- The `metaphor schema generate --force` step is required for entity *removal* (the default generate is additive-only and leaves stale generated files); the 29 orphaned `*sales_invoice*` generated files are deleted by hand after the forced regen (regen-safe: the schema YAML is gone, so they cannot recur).

## Verification

- `backbone-billing`: `ar_seam.rs` (2) + `billing_golden_cases.rs` (6) green.
- `backbone-selling`: full suite green (31 passed / 0 failed / 6 ignored) both before and after the DROP migration.
- `grep -rn "SalesInvoice|GlPost|backbone_gl_posting" src/ tests/` → empty. Selling references invoice/GL-posting nowhere.
