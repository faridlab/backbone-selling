# ADR-004: The selling↔inventory delivery seam (order-to-cash + fulfillment, end-to-end)

**Status**: Accepted — **Applied 2026-07-04** (the marquee multi-module seam, proven end-to-end)
**Deciders**: Farid (owner), build session 2026-07-04
**Related**: ADR-002 (GL seam), ADR-003 (order status model), inventory ADR-001/002,
`docs/erp/extension-contract.md` §5, `docs/erp/supply-chain.md`

## Context

Selling proved the revenue GL seam; inventory proved the COGS/asset GL seam. This ADR records the
**cross-module fulfillment seam** that ties them together end-to-end: a confirmed Sales Order gets
delivered by inventory (COGS posts) and billed by selling (revenue posts), and the order completes.
This is the first time three modules (selling, inventory, accounting) collaborate on one business
transaction — the proof that the decomposition composes, not just isolates.

## Decision

1. **Every cross-module hop is a serialized envelope mapped by an ACL — zero normal Cargo edges.**
   - Selling emits a `DeliveryRequestEnvelope { order_id, company_id, customer_id, lines[] }`
     (`build_delivery_request`); a fulfillment/composition layer maps it into inventory's
     `DeliveryRequested` (adding the **warehouse + GL accounts inventory owns** — selling stays
     ignorant of them) → inventory creates a **draft** Delivery Note.
   - Inventory `submit_delivery_note(sink)` writes the SLE + Bin and emits the COGS `AccountingPost`
     into the real ledger, plus a `StockDelivered { source_so_id, total_cogs }` event.
   - The composition routes `StockDelivered` → selling `mark_delivered(order, lines)` → advances
     `delivered_qty`.
   The shipped selling library has **no normal dependency on inventory** (`cargo tree -e normal -i
   backbone-inventory` is empty); inventory is a dev-dependency for the seam test only.
2. **Delivery is now a live watermark (ADR-003 amended).** `confirm_sales_order` → `to_deliver_and_bill`;
   the order recomputes to `to_deliver` (billed, awaiting delivery) / `to_bill` (delivered, awaiting
   billing) / `completed` (both). `completed` requires **fully billed AND fully delivered** — an
   order can no longer complete while undelivered.
3. **Physical + financial stay decoupled and eventually consistent.** The delivery's COGS post and
   the invoice's revenue post are independent `AccountingPost`s into the same ledger; either can
   happen first; each is idempotent on its own `source_id`.

## Consequences

- **Proven, not asserted:** `tests/delivery_seam.rs` runs the full round-trip — inventory receives
  stock, selling confirms an order, emits a delivery request, inventory delivers (COGS journal `Dr
  COGS 1,000 · Cr Inventory 1,000`), selling records the delivery, then bills + posts (revenue
  journal `Dr A/R 1,665,000 · Cr Revenue 1,500,000 · Cr PPN 165,000`), the order reaches
  `completed`, three journals exist, and the Bin drains to exactly 0.
- **Extension-contract §5 discharged for the seam:** `scripts/delivery_seam_roundtrip.sh` regenerates
  **both** modules and asserts every ACL/consumer file is byte-identical and the seam stays green —
  the consumer-rule-survives-regen round-trip the inventory completeness council parked.
- All three schemas co-locate in one database (`selling.*`, `inventory.*`, `accounting.*`) as a
  composed service would run them; the shared `gl_posting_state` enum carries the union of both
  modules' variants.
- Residual / parking lot: partial deliveries across multiple Delivery Notes (the watermark supports
  it; not yet tested); a real event bus + fulfillment service to own the ACL in production (today the
  test is the composition root); stock soft-reservation on Sales Order confirmation.
