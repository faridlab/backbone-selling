# ADR-005: The selling↔billing invoice seam (order-to-cash billing, end-to-end)

**Status**: Accepted — Applied 2026-07-05 (proven end-to-end; retires selling's simulated invoice leg)
**Deciders**: Farid (owner), build session 2026-07-05
**Related**: ADR-002 (GL-posting seam), ADR-003 (order-status model), ADR-004 (delivery seam),
billing ADR-001/002, buying ADR-001/002 (the A/P seam this mirrors), `docs/erp/extension-contract.md` §5

## Context

Selling proved order-to-cash intent + a revenue GL post, but *folded the Sales Invoice in itself*
(`create_invoice_from_order` + `post_sales_invoice` — the simulated leg, "until billing splits out",
ADR-001). Billing now owns AR invoicing. This ADR records the **invoice seam**: a confirmed Sales
Order is billed by a *real* billing Sales Invoice, and the order's `billed_qty` advances from billing's
event — the order-to-cash mirror of the buying↔billing A/P seam (`PurchaseInvoicePosted → mark_billed`).

## Decision

1. **Every cross-module hop is a serialized envelope mapped by an ACL — zero normal Cargo edges.**
   - Selling emits `InvoiceRequestEnvelope { order_id, company_id, customer_id, currency, lines[] }`
     (`build_invoice_request` — the un-invoiced remainder `quantity − billed_qty` per line, with the
     unit price); a composition ACL maps it into billing's `NewSalesInvoice` (adding the A/R + revenue
     accounts billing/accounting own) → billing raises + posts the invoice.
   - Billing posts the revenue journal (`Dr A/R · Cr Revenue`) into the real ledger and emits
     `SalesInvoicePosted { source_so_id, billed_lines }` (billed_lines added this session, symmetric
     with `PurchaseInvoicePosted`); the composition routes it → selling `mark_invoiced` → advances
     `billed_qty`.
   The shipped selling library has **no normal dependency** on billing or accounting (dev-deps only;
   the envelope is the wire contract). **Selling posts NO revenue in the composed flow** — the seam
   test asserts selling emits no `SalesInvoicePosted` — retiring the simulated leg (which remains as
   dead-in-composed-flow legacy code, deletion is a separate cleanup).
2. **`mark_invoiced` is BOUNDED (council 2026-07-05).** It routes through a capacity-checked,
   `FOR UPDATE`-serialized allocation capped at each line's `quantity`, and **rejects** an over-bill
   (`OverBilled`) — the direct mirror of buying's `mark_billed`/`allocate`. The line bound alone (the
   upstream remainder) does not protect the watermark: `billed_qty` advances only at post time, so two
   invoice requests can each read `billed_qty = 0` and both bill the full remainder. Serializing the
   *writer* is what closes the race; without it, `billed_qty` runs past `quantity` (booking revenue
   beyond the order) while `recompute_order_status` (`billed_qty ≥ quantity`) silently masks it as
   `completed`. Aggregate-by-item, fill-in-order — correct for duplicate-item orders.
3. **Status stays a computed invariant over the two watermarks** (ADR-003): `completed` iff every line
   is fully billed AND fully delivered. `mark_invoiced` (billed) and `mark_delivered` (delivered) are
   the two independent advances; the invoice seam supplies the billed one from a real invoice.

## Consequences

- **Proven, not asserted:** `tests/invoice_seam.rs` runs selling → billing → accounting → selling
  (ISEAM-1: order 10 @ 100,000 → invoice request → billing posts `Dr A/R 1,000,000 · Cr Revenue
  1,000,000` → `SalesInvoicePosted` → `mark_invoiced` → `billed_qty = 10`; selling emits no revenue).
  ISEAM-2 (over-bill refused: a second full `mark_invoiced` → `OverBilled`, watermark untouched) and
  ISEAM-3 (duplicate-item: two lines 6+4, billing 12 refused, 10 fills both) lock the bound.
- **Extension-contract §5 discharged:** `scripts/invoice_seam_roundtrip.sh` regenerates **both** modules
  and asserts every ACL/consumer file is byte-identical and the seam stays green.
- This completes **order-to-cash module-to-module**: SO → (delivery seam) → Delivery Note → (invoice
  seam) → real Sales Invoice → revenue GL → settlement → bank clearing, with no simulated legs.
- Parking lot (with gates): `mark_delivered` is the identical uncapped twin — NOT inventory-guaranteed
  (inventory gates stock, not ordered qty); fix next via a shared capped allocate. The legacy invoice
  leg (`create_invoice_from_order`/`post_sales_invoice`) — delete in a cleanup PR after a call-site
  sweep; route its `advance_billing_watermarks` through the same cap before ever re-wiring it live.
  At-least-once bus redelivery of `SalesInvoicePosted` — the `FOR UPDATE` cap makes the over-bill
  consequence idempotent (redelivery hits the cap and rejects); true dedup is a messaging concern.
