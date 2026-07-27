# Changelog

All notable changes to **backbone-selling** are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the authoritative rationale for each entry
lives in the linked [ADR](docs/adr/). The module is pre-1.0 and unversioned — entries are grouped by
the date the change was applied.

## [Unreleased]

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
