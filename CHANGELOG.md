# Changelog

All notable changes to **backbone-selling** are recorded here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the authoritative rationale for each entry
lives in the linked [ADR](docs/adr/). The module is pre-1.0 and unversioned — entries are grouped by
the date the change was applied.

## [Unreleased]

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
