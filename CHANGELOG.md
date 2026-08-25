# Changelog

All notable changes to **backbone-selling** are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the authoritative rationale for each entry
lives in the linked [ADR](docs/adr/). The module is pre-1.0 and unversioned — entries are grouped by
the date the change was applied.

## [Unreleased]

### Added — the unit-cost margin snapshot, the expense-reinvoice link, and the delivery-carrier registry ([ADR-008], 2026-08-25)

Odoo `sale.order.line.purchase_price` semantics, adapted. Confirm stamps each live order line's
`unit_cost` from a host-supplied cost source; margin is a read-time compute over those snapshots
(never persisted, never writable). Selling also gains the rebill-expenses-to-the-customer link
(the host billing adapter's pull surface) and a registry-only delivery-carrier master:

- **Added (schema + migration `20260825000100_selling_margin_delivery_reinvoice`):**
  `unit_cost NUMERIC(18,6)` on `sales_order_items` (confirm-only writer), the
  `ExpenseReinvoiceLink` table (`state` pending/invoiced, partial unique `(order_id, expense_id)`
  over live rows — the double-bill guard), the `DeliveryCarrier` registry, and
  `delivery_carrier_id` + `tracking_ref` on `sales_orders`. CHECK constraints land NOT VALID then
  VALIDATE; no backfill.
- **Added (port):** `UnitCostPort` / `UnitCostRequest` / `ItemUnitCost` / `UnitCostError` — the
  catalog standard-cost seam (DTOs as the wire contract, zero cargo edge), plus the
  all-NULL `NoUnitCostPort` for compositions that never confirm orders.
- **Breaking (public API):** `confirm_sales_order(order_id, company_id, costs: &dyn UnitCostPort)`
  now takes the cost port as a third argument, and `create_guarded_selling_routes` takes
  `unit_cost: Arc<dyn UnitCostPort>` as a fourth REQUIRED argument. The port runs BEFORE the
  transaction; the stamp + the unchanged draft-guard run as one unit of work, so a losing confirm
  rolls its stamp back. Port `Err`, an omitted item, or a negative cost each REFUSE the confirm
  (`cost_rejected`, 422); a NULL cost PROCEEDS and reads as honest absence.
- **Added (public API):** `order_margin_view` (per-line `margin` / `marginPercent` + the costed-
  subset rollup with coverage counters; `line_margin = line_amount − unit_cost·qty` —
  `unit_cost` NULL ⇒ margin NULL, never zero; negative margins are legal),
  `attach_expense_reinvoice` / `list_expense_reinvoices` / `mark_expense_reinvoice_invoiced`
  (a double mark is a LOUD refusal; `expense_id` is taken on faith — the host validates the
  expense), and the carrier verbs `create_delivery_carrier` / `update_delivery_carrier` /
  `list_delivery_carriers` / `set_order_delivery` (deactivate-not-delete; carrier/tracking are
  fulfillment metadata — writable on draft AND confirmed orders, refused on cancelled).
- **Added (routes):** `GET /sales-orders/:id/margin`,
  `GET+POST /sales-orders/:id/expense-reinvoices`, `POST /expense-reinvoices/:id/mark-invoiced`,
  `POST+GET /delivery-carriers`, `PATCH /delivery-carriers/:id`,
  `POST /sales-orders/set-delivery`; `CreateSalesOrderBody.deliveryCarrierId`; the generic
  read routers for the two registry entities mount at their framework paths.
- **New stable error codes (422/404):** `cost_rejected` (carries the port's code verbatim),
  `carrier_not_found`, `duplicate_carrier_name`, `reinvoice_not_found`, `duplicate_reinvoice`,
  `invalid_reinvoice_amount`.
- **No new events:** the stamp rides `SalesOrderConfirmed`; the reinvoice link is a pull model
  (no outbox, no push) — the billing adapter pulls the pending list and marks after its post acks.

### Added — the invoicing-policy engine, the quotation machine, and the order's exits ([ADR-007], 2026-08-24)

Odoo `sale.order` semantics, adapted. Per-line invoicing policy (`order` — default — or
`delivery`), computed invoiceability, downpayment lines, a full quotation state machine, order
cancel + line-freeze guards, quotation templates, and the opportunity link:

- **Added (schema + migration `20260824000100_selling_invoice_policy_and_templates`):**
  `invoice_policy` + `is_downpayment` on quotation and order lines, `status_reason` +
  `opportunity_id` on quotations, the `QuotationTemplate` master (RLS-fenced,
  unique `(company_id, name)`), the `invoice_policy` enum.
- **Added (public API):** `SellingWriteService::{send_quotation, reject_quotation,
  cancel_quotation, redraft_quotation, cancel_sales_order, update_order_line,
  order_invoice_view, quotation_invoice_view, create_quotation_template,
  list_quotation_templates}`; the `QuotationSent` / `QuotationRejected` / `QuotationCancelled` /
  `QuotationReDrafted` / `SalesOrderCancelled` events; the four invoice-status read DTOs.
- **Added (routes):** `POST /quotations/{send,re-draft,reject,cancel}`,
  `POST /sales-orders/cancel`, `PATCH /sales-orders/lines/:id`,
  `GET {/sales-orders,/quotations}/:id/invoice-status`, `POST+GET /quotation-templates` — all
  tenant-scoped behind `company_auth`.
- **The single-source invariant:** `qty_to_invoice` / invoice-status / the billing remainder / the
  billed-watermark bound / the status rollup all consume ONE canonical basis
  (`delivery ⇒ delivered_qty`, else `quantity`), so delivery-policy orders no longer strand in
  `to_deliver_and_bill`. `qty_to_invoice` / `invoice_status` are read-time computes — never
  persisted, not writable.
- **Breaking (error surface):** the hook spec's invoice-era rules R3/R4/R6/R7 (and the stale
  SalesInvoice machine + events) are removed per ADR-006's retirement; new stable codes
  `invalid_transition`, `quotation_ordered`, `order_billed`, `order_line_frozen`,
  `template_not_found`, `duplicate_template_name` (all 422).

### Fixed — the migration chain applies cleanly to a fresh database (2026-08-23)

The invoice-business exit ([ADR-006]) removed the `sales_invoices` / `sales_invoice_items` tables
and their CREATE migrations from the chain, but two earlier invoice-era migrations still referenced
those tables unconditionally — so applying the full chain to a **fresh** database failed:

- `20260426220020_sales_invoice_idr_only_check` — `ALTER TABLE selling.sales_invoices` errored with
  `relation "selling.sales_invoices" does not exist` (the constraint's target table never comes into
  being on a fresh chain).
- `20260722000100_child_tables_company_rls` — the `sales_invoice_items` fencing block errored the
  same way; the other three child-table blocks were unaffected.

Both invoice-era blocks are now guarded with `IF to_regclass(...) IS NOT NULL` (and the IDR-only
CHECK's down side likewise), so the chain is valid on a fresh database **and** on databases
provisioned before the exit, where the tables still existed and the statements still apply.
`20260728000100_drop_sales_invoice_tables` already used `DROP TABLE IF EXISTS` and needed no change.
No schema shape changes: on a fresh database the guarded statements are skipped, exactly matching
the post-drop end state the exit produced.

### Removed — BREAKING: selling exits the invoice business ([ADR-006], 2026-07-27)

Billing now owns AR invoicing + revenue recognition, end to end. Selling's own invoice write/post path,
its GL seam, the `SalesInvoice` entity, and the tables are removed (supersedes ADR-001 / ADR-002):

- **Removed (public API):** `SellingWriteService::{create_sales_invoice, create_invoice_from_order,
  post_sales_invoice, build_revenue_post}`, the `NewSalesInvoice` / `PostOutcome` types, the
  `SalesInvoiceIssued` / `SalesInvoicePosted` events, the `selling_gl` GL-posting re-exports, the
  `backbone-gl-posting` Cargo dependency, the generated `SalesInvoice*` entity/DTO/service/route
  surface, and the `sales_invoices` / `sales_invoice_items` tables
  (migration `20260728000100_drop_sales_invoice_tables`).
- **Kept:** `build_invoice_request` (outbound `InvoiceRequestEnvelope`) + `mark_invoiced` (advances
  `billed_qty`) — selling's entire invoice surface is now the billing-owned seam (ADR-005).
- Revenue/invoice coverage relocated to `backbone-billing` (`tests/ar_seam.rs`, `billing_golden_cases.rs`).
- `tests/delivery_seam.rs` now stops at `to_bill`; `tests/order_to_cash.rs` + `tests/gl_posting_seam.rs`
  + the 7 invoice golden cases are removed from selling.

### Added — selling↔inventory delivery seam ([ADR-004], applied 2026-07-04)

- `SellingWriteService::build_delivery_request` — builds the cross-module `DeliveryRequestEnvelope`
  for a confirmed order and emits the `DeliveryRequested` domain event. Zero normal Cargo edge on
  `backbone-inventory` (an ACL/composition layer maps the envelope into inventory's own
  `DeliveryRequested`).
- `SellingWriteService::mark_delivered` — inbound handler for inventory's `StockDelivered`; advances
  each line's `delivered_qty` and recomputes order status.
- `DeliveryRequestEnvelope` / `DeliveryRequestLine` wire types and the `SellingEvent::DeliveryRequested`
  variant in `selling_events`.
- `delivered_qty` watermark on `SalesOrderItem` (schema + migration + seed + DTO + exported type).
- `tests/delivery_seam.rs` — full selling↔inventory↔accounting round-trip (COGS + revenue journals,
  order → `completed`, Bin drains to 0). `scripts/delivery_seam_roundtrip.sh` — regenerates both
  modules and asserts every seam ACL/consumer file is byte-identical (extension-contract §5).
- Golden case **DSEAM-1** and the §5 round-trip case in `docs/business-flows/golden-cases.md`.

### Changed — order-status model amended ([ADR-003] amendment, 2026-07-04)

- `confirm_sales_order` now advances a draft order to `to_deliver_and_bill` (was `to_bill`).
- New `recompute_order_status` derives status from both watermarks: `completed` iff every line is
  fully billed **and** fully delivered; else `to_deliver` / `to_bill` / `to_deliver_and_bill`. An
  order can no longer reach `completed` while undelivered. The whole 7-state model is now live (the
  delivery band is no longer dark).
- Handbook, README, PRD/FSD/BRD, glossary, and extension-guide updated to reflect the live delivery
  seam.

### Fixed — delivery over-delivery + outbox drift (2026-07-27)

- `SellingWriteService::mark_delivered` is now capacity-checked and `FOR UPDATE`-serialized,
  rejecting `OverDelivered` — the delivery twin of `mark_invoiced`. Previously a single
  `StockDelivered` for more than was ordered pushed `delivered_qty` past `quantity`, and
  `recompute_order_status` (`delivered_qty >= quantity`) silently masked it as the delivered band
  (and could mark the order `completed` for stock never ordered). See [ADR-004] amendment.
- `selling.outbox_events` now carries `company_id` (migration `20260727000100`), so
  `backbone-outbox` v2.7.6 `multi_tenant` `stage()` writes succeed — the column was missing after the
  v2.7.6 pin bump, breaking `build_delivery_request`.

### Docs — cross-module FK contract + council outcome (2026-07-27)

- `docs/extension-guide.md` now states cross-module FKs are logical
  (`@exclude_from_foreign_key_check`), not DB-enforced — integrators must not assume referential
  integrity across schemas; enforce correspondence at the composition/ACL layer.
- `docs/council/2026-07-27-module-backbone-selling-maturity.md` gained an execution post-script: **#1
  shipped (v0.5.4)**; **#2/#4 are blocked** by the `metaphor-codegen` template (the generated
  `SellingModule` + the generated ES scaffold can't be reshaped/deleted at module level); **#3 is a
  deferred** cross-module architecture migration. The council's diagnosis holds; 3 of 4 fixes are
  upstream of this module.

## [2026-07-04] — Initial selling module

The order-to-cash foundation: Quotation → Sales Order → Sales Invoice, revenue recognised by emitting
a balanced posting into `backbone-accounting`.

### Added

- Schema-YAML SSoT for the eight selling entities and the generated 4-layer DDD code (entities, DTOs,
  repositories, services, handlers, routes), plus Postgres migrations including the IDR-only invoice
  `CHECK` guard.
- The hand-written selling core: the validated `SellingWriteService`, the GL-posting seam
  (`AccountingPostEnvelope` + `GlPostSink` ACL — idempotent, balanced, IDR-only; [ADR-002]) and the
  domain-event extension surface ([ADR-001]).
- The 7-state sales-order status model ([ADR-003]) — billing band live, delivery band declared but
  dark pending `backbone-inventory`.
- The golden-case oracle, GL-seam proof, integrity probes, extension-contract test, and the regen
  round-trip script; the full handbook, ADRs, and business-flow docs.

[Unreleased]: #unreleased
[ADR-001]: docs/adr/ADR-001-selling-boundary.md
[ADR-002]: docs/adr/ADR-002-gl-posting-seam.md
[ADR-003]: docs/adr/ADR-003-order-status-model.md
[ADR-004]: docs/adr/ADR-004-delivery-seam.md
[ADR-005]: docs/adr/ADR-005-invoice-seam.md
[ADR-006]: docs/adr/ADR-006-selling-exits-invoice-business.md
[ADR-007]: docs/adr/ADR-007-invoicing-policy-engine.md
[ADR-008]: docs/adr/ADR-008-margin-and-registry.md
