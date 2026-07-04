# ADR-003: Sales-order status model (7-state; all bands now live — amended by ADR-004)

**Status**: Accepted — **Applied 2026-07-03** (completeness council); **amended 2026-07-04** by
[ADR-004](ADR-004-delivery-seam.md) — the delivery band went live when `backbone-inventory` landed
(see the Decision note below; the original 2026-07-03 table is kept as the historical record).
**Deciders**: Farid (owner), completeness council 2026-07-03
**Related**: module brief (`docs/erp/modules/backbone-selling.md`), ADR-001, `docs/erp/supply-chain.md`

## Context

The first build of `backbone-selling` shipped a 5-state `SalesOrderStatus`
`{draft, confirmed, fulfilled, closed, cancelled}` — a silent downgrade of the module brief's
**7-state** model, with no ADR to justify it. The completeness council flagged this: the brief's
states `{draft, to_deliver, to_bill, to_deliver_and_bill, completed, cancelled, closed}` are not
cosmetic — they *encode the delivery and billing watermarks as states*, so collapsing them dropped
domain intent. The 5-state `confirmed`/`fulfilled` blurred "awaiting billing" vs "awaiting delivery"
and had no path to a fully-billed terminal.

`backbone-inventory` (which owns Delivery Notes and advances `delivered_qty`) is not built in this
workspace, so the delivery half of the model cannot be exercised or tested yet.

## Decision

**Restore the brief's 7-state model** and split the transitions into two bands:

| State | Meaning | Active now? |
|-------|---------|-------------|
| `draft` | Being prepared | yes |
| `to_bill` | Confirmed; awaiting billing only (delivered, or delivery not tracked yet) | **yes** *(2026-07-03: `confirm_sales_order` set this; since ADR-004 confirm sets `to_deliver_and_bill` and `recompute_order_status` reaches `to_bill` once delivered)* |
| `completed` | Fully delivered **and** fully billed | **yes** *(2026-07-03: `advance_billing_watermarks` on `billed_qty >= quantity`; since ADR-004 `recompute_order_status` also requires `delivered_qty >= quantity`)* |
| `closed` | Manually closed | yes (manual) |
| `cancelled` | Voided | yes |
| `to_deliver` | Confirmed; awaiting delivery only (fully billed) | *(2026-07-03: inventory-gated)* → **live** since ADR-004 |
| `to_deliver_and_bill` | Confirmed; awaiting both | *(2026-07-03: inventory-gated)* → **live** since ADR-004; `confirm_sales_order` now sets this |

- **Both bands are live now (updated 2026-07-04, ADR-004 — inventory landed).** `confirm_sales_order`
  → `to_deliver_and_bill`. The order recomputes from its two watermarks: `completed` iff every line is
  fully billed AND fully delivered; else `to_deliver` (billed, awaiting delivery) / `to_bill`
  (delivered, awaiting billing) / `to_deliver_and_bill` (awaiting both). `billed_qty` advances on
  `post_sales_invoice`; `delivered_qty` advances on `mark_delivered` (driven by inventory's
  `StockDelivered` — see ADR-004). An order can no longer complete while undelivered.

## Consequences

- The state machine again matches the brief; `SGC-7` and `OTC-1` assert the lifecycle
  (`to_deliver_and_bill` on confirm → `to_deliver`/`to_bill` → `completed`).
- **Realized 2026-07-04 (ADR-004):** `backbone-inventory` landed, `confirm_sales_order` now branches
  to `to_deliver_and_bill`, and `mark_delivered` (driven by inventory's `StockDelivered`) advances
  `delivered_qty` and drives the delivery band — exactly the additive, no-enum-change path predicted
  here. `DSEAM-1` (`tests/delivery_seam.rs`) proves it end-to-end.
- The bands were declared before they were reachable, so making them live required **no enum change** —
  the discipline the council called out paid off.
