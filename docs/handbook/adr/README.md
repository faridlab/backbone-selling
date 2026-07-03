# Architecture Decision Records

One decision per record: context, decision, alternatives, consequences. **Immutable once
accepted** — to change a decision, write a new ADR that supersedes the old one and update its
Status line; never edit an accepted decision in place.

Selling keeps two ADR sets. **Framework ADRs** (below) explain how *any* Backbone module works.
**Selling-domain ADRs** (in [`../../adr/`](../../adr/)) explain why *this* module is shaped the way
it is.

## Framework decisions

| ADR | Decision | Status |
|-----|----------|--------|
| [0001](adr-0001-schema-yaml-ssot.md) | Schema YAML is the single source of truth | Accepted |
| [0002](adr-0002-generic-crud.md) | CRUD is inherited from generics, not written per entity | Accepted |
| [0003](adr-0003-custom-markers.md) | Regen-safety via CUSTOM markers and `user_owned` | Accepted |

## Selling-domain decisions

| ADR | Decision | Status |
|-----|----------|--------|
| [ADR-001](../../adr/ADR-001-selling-boundary.md) | Selling owns order-to-cash intent + revenue recognition; it is a GL producer, never the ledger; holds no masters | Accepted (Applied 2026-07-03) |
| [ADR-002](../../adr/ADR-002-gl-posting-seam.md) | The GL-posting seam: a serialized `AccountingPostEnvelope` + `GlPostSink` ACL; idempotent, balanced, IDR-only | Accepted (Applied 2026-07-03) |
| [ADR-003](../../adr/ADR-003-order-status-model.md) | 7-state sales-order model; billing band live, delivery band inventory-gated | Accepted (Applied 2026-07-03) |
