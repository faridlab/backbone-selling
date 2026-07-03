# ADR-001: Selling owns order-to-cash intent + revenue recognition; it is a GL producer, never the ledger

**Status**: Accepted — **Applied 2026-07-03**
**Deciders**: Farid (owner), build session 2026-07-03
**Related**: `docs/erp/supply-chain.md`, `docs/erp/gl-posting-contract.md`, ADR-002 (the GL seam)

## Context

`backbone-selling` is the first Tier-2 producer built. The supply-chain carve splits the
order-to-cash pipeline across modules: Selling owns *intent* (Quotation → Sales Order), Inventory
owns the physical move + COGS post, and Billing posts revenue. The owner's build decision for this
module folded **revenue recognition (Sales Invoice)** into selling so the marquee GL-posting seam
could be proven now, rather than waiting for a separate billing module. (When billing is split out,
the invoice + revenue post move there; the envelope contract in ADR-002 makes that a lift, not a
rewrite.)

Selling holds **no masters**. Customer is a logical FK to `party.Party`; every line's item is a
logical FK to `catalog.Item`; company/branch are logical FKs to `organization`; the GL account
references (A/R control, income, PPN Output) are logical FKs to `accounting.Account`. None are DB
constraints into another schema (`@exclude_from_foreign_key_check`), so selling has **zero
horizontal Cargo edges** and never joins across a module boundary.

## Decision

1. **Three documents, one pipeline.** `Quotation → SalesOrder → SalesInvoice`, each with line
   children. Quotation/Order carry intent and totals; only the **invoice** recognises revenue and
   emits to the GL. Order↔Quotation and Invoice↔Order links are intra-module FKs; everything
   cross-module is a logical FK.
2. **Money is computed server-side, never trusted from the client.** The validated write path
   computes `line_amount = money(qty·price) − discount` and the document totals
   (`subtotal / tax_amount / total`) at 2dp half-up. Generic CRUD create/update/delete is **not
   mounted** on the guarded surface (`create_guarded_selling_routes`) — a caller cannot post an
   invoice with an inconsistent `total`, no lines, or a server-owned field it should not set.
3. **Tax is a supplied rate here, deferred to `backbone-tax` for real computation.** The invoice
   carries a single `tax_rate` and computes one PPN amount on the subtotal. Multi-rate,
   effective-dated, inclusive tax is `backbone-tax`'s job; wiring it in is future work over the same
   envelope pattern (the tax lines would arrive pre-computed and be attached to the post).
4. **Revenue recognition state is separate from document state.** `status`
   (draft/submitted/paid/…) is the document lifecycle; `posting_state` (pending/posted/failed) is
   the GL reconciliation state, set from the posting ack/event (ADR-002). They move independently.

## Consequences

- The order-to-cash happy path and its numeric oracle are locked by `tests/selling_golden_cases.rs`
  (7 cases) and the guarded surface by `tests/integrity_probes.rs` (5 cases).
- Selling is independently composable: it needs only a Postgres pool and a `GlPostSink` to run; it
  imports no sibling module.
- When `backbone-billing` is introduced, the invoice + revenue post relocate there unchanged; the
  `AccountingPostEnvelope` contract is the seam that makes that non-breaking.
- Deferred (parking lot): Delivery/COGS post (belongs to inventory), credit-limit enforcement,
  product bundles, real `backbone-tax` wiring, payment application against `outstanding_amount`.
