# ADR-003: Sales-order status model (7-state; billing active, delivery inventory-gated)

**Status**: Accepted — **Applied 2026-07-03** (completeness council)
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
| `to_bill` | Confirmed; awaiting billing only (no delivery tracking yet, or fully delivered) | **yes** — `confirm_sales_order` sets this |
| `completed` | Fully delivered **and** fully billed | **yes** — `advance_billing_watermarks` sets this once every line's `billed_qty >= quantity` |
| `closed` | Manually closed | yes (manual) |
| `cancelled` | Voided | yes |
| `to_deliver` | Confirmed; awaiting delivery only (fully billed) | **inventory-gated** — reserved for when backbone-inventory lands |
| `to_deliver_and_bill` | Confirmed; awaiting both | **inventory-gated** |

- **Billing band is live now:** `draft → to_bill → completed` is driven by the in-module invoice
  (`create_invoice_from_order` → `post_sales_invoice` advances `SalesOrderItem.billed_qty` and closes
  the order when fully billed).
- **Delivery band is intentionally dark:** `to_deliver` / `to_deliver_and_bill` (and the
  `delivered_qty` watermark) require Delivery Notes from `backbone-inventory`. They exist in the enum
  as the documented target so the model doesn't have to change when inventory arrives; no code reaches
  them yet. This is a *declared* deferral, not a silent one.

## Consequences

- The state machine again matches the brief; `SGC-7` and `OTC-1` assert the live billing band
  (`to_bill`, `completed`).
- When `backbone-inventory` lands, `confirm_sales_order` will branch to `to_deliver_and_bill` and a
  `DeliveryNoteSubmitted` handler will advance `delivered_qty` and drive the delivery band — additive,
  no enum change.
- Unreachable-for-now states are acceptable and explicitly documented here (avoids the silent-scope
  loss the council called out).
