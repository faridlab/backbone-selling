# FSD — backbone-selling

> Functional Spec. Tier 2 · Supply Chain. Date: 2026-07-03. Implementation-facing; maps rules
> (BRD) to entities, services, endpoints, states, and integration seams.

## 1. Entities (schema/models/*.model.yaml — SSoT)

| Entity | Table | Notes |
|--------|-------|-------|
| Quotation / QuotationItem | `selling.quotations` / `_items` | offer + lines; totals computed |
| SalesOrder / SalesOrderItem | `selling.sales_orders` / `_items` | `quotation_id` link; item `billed_qty` watermark |
| SalesInvoice / SalesInvoiceItem | `selling.sales_invoices` / `_items` | `sales_order_id` link; item `sales_order_item_id` link; GL account refs + `posting_state`/`journal_id`/`accounting_post_id` |
| SalesTeam / SalesPersonAllocation | `selling.sales_teams` / `sales_person_allocations` | commission attribution on an order |

All cross-module ids are logical FKs (`@exclude_from_foreign_key_check`): `customer_id`→party,
`item_id`→catalog, `company_id`/`branch_id`→organization, `receivable/revenue/tax_output_account_id`
→accounting, `sales_person_id`→sapiens.

## 2. Services (application/service — hand-authored, user_owned)

- `SellingWriteService` — validated writes + orchestration:
  - `create_quotation` / `create_sales_order` / `create_sales_invoice` (server-side money; one tx)
  - `accept_quotation` → emits `QuotationAccepted`
  - `convert_quotation_to_order` (Quote→Order: copy + link; quotation → `ordered`)
  - `confirm_sales_order` → `to_bill`, emits `SalesOrderConfirmed`
  - `create_invoice_from_order` (Order→Bill: copy lines, link `sales_order_item_id`), emits `SalesInvoiceIssued`
  - `build_revenue_post` (pure, balanced envelope) + `post_sales_invoice(sink)` (emit → reconcile →
    advance `billed_qty` → `SalesInvoicePosted`); idempotent
  - `sales_order_ref` (exported `SalesOrderRef` DTO)
- `selling_gl` — outbound GL port: `AccountingPostEnvelope`, `GlPostLine`, `GlPostSink`, ack/reject.
- `selling_events` — domain events + `SellingEventSink` + exported `SalesOrderRef`/`QuotationRef`.
- `consumer_credit_rule_custom` — reference consumer extension (extension-contract §5).

## 3. HTTP surface (presentation/http/guarded_routes.rs)

`create_guarded_selling_routes(&SellingModule, pool)` mounts **read + validated create** only
(no generic mutation): `POST /quotations`, `POST /sales-orders`, `POST /sales-orders/confirm`,
`POST /sales-invoices`, plus read routes for all documents. Posting is service/job-driven (needs a
`GlPostSink` from the composing service), not an HTTP route.

## 4. State machines

- **Quotation:** `draft → sent → accepted → ordered` (+ rejected/expired/cancelled).
- **SalesOrder (ADR-003):** `draft → to_bill → completed`; `closed`/`cancelled`; `to_deliver`/
  `to_deliver_and_bill` inventory-gated.
- **SalesInvoice:** `draft → submitted → (partially_paid) → paid`; `cancelled`. `posting_state`
  (pending → posted | failed) is an independent GL-reconciliation axis.

## 5. Integration seams

- **Outbound GL (proven):** `post_sales_invoice` → `GlPostSink` → accounting `PostingService`
  (envelope → PostingRequest ACL). Idempotent on invoice id; concurrency-safe. See ADR-002,
  `tests/gl_posting_seam.rs`.
- **Outbound events:** `SellingEventSink` publishes the 4 domain events; consumers subscribe via
  `*_custom.rs` (extension-contract §5; regen-proven by `scripts/regen_roundtrip.sh`).
- **Inbound (future):** `ItemCreated/Updated` + `PartyCreated/Updated` → local projections;
  `DeliveryNoteSubmitted` (inventory) → `delivered_qty` — when inventory lands.

## 6. Test oracle

`selling_golden_cases` (8, money/validation/currency), `integrity_probes` (5, guarded surface),
`gl_posting_seam` (4, real ledger + idempotency + concurrency + rejection), `extension_contract`
(2, consumer rule on events), `order_to_cash` (2, Quote→Order→Invoice→post + watermarks).
`scripts/regen_roundtrip.sh` proves regen-survival. **21 tests + the round-trip.**
