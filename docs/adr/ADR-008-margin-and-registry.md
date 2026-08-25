# ADR-008: The unit-cost margin snapshot, the expense-reinvoice link, and the delivery-carrier registry

**Status**: Accepted — **Applied 2026-08-25**
**Related**: [ADR-002](ADR-002-gl-posting-seam.md) (the port idiom this seams follows), [ADR-005](ADR-005-invoice-seam.md) (the billing-owned invoice the reinvoice pull feeds), [ADR-006](ADR-006-selling-exits-invoice-business.md) (why returns are not netted here), [ADR-007](ADR-007-invoicing-policy-engine.md) (the read-time-compute precedent)

## Context

Three gaps, one release. Selling could price and bill but could not answer *"what did this order
earn?"* — no cost anywhere on an order line, so no margin. Expenses a company wanted to rebill to
its customer had no first-class link from the order to the billing side. And delivery had no
carrier master at all — a tracking number lived nowhere because there was nothing to key it to.

Odoo's answer to the first is `sale.order.line.purchase_price`: a **per-line standard cost frozen
at confirmation**. The freeze is the point — a margin computed against today's catalog cost
rewrites history every time a cost price changes, so analytics drift silently long after the sale
is immutable.

## Decision

### 1. The unit-cost snapshot: a confirm-time stamp through a host-owned port

`SalesOrderItem.unit_cost NUMERIC(18,6) NULL` (schema `@non_negative`). The **only writer** is the
confirm flow: `confirm_sales_order(order_id, company_id, costs: &dyn UnitCostPort)` — a breaking
3-arg signature whose third argument is the cost source, supplied by the composing host
(`create_guarded_selling_routes` takes `Arc<dyn UnitCostPort>` as a required fourth argument).
`NoUnitCostPort` resolves everything to NULL for compositions that never confirm orders.

The port is the `CartPricingPort` idiom (ADR-002): DTOs as the wire contract, per-call `&dyn`, zero
cargo edge. Selling does not know the cost is catalog standard cost; the host's adapter does.

Flow, and why in that order:

1. read the order's live `(line_id, item_id)` pairs (request scope);
2. ask the port for the **distinct** items' costs — **before any transaction**, so no network call
   runs inside the DB tx and draft lines are not locked across the port;
3. in ONE transaction: stamp every line via an `unnest` join, then run the **unchanged**
   draft-guard `UPDATE`. A losing guard (0 rows — wrong tenant, absent id, or already confirmed)
   rolls the stamp back with it; a loser of two concurrent confirms never leaves a cost on a
   non-confirmed order.

**Port-failure rule (explicit):** a port `Err`, a requested item MISSING from the response, or a
NEGATIVE cost each REFUSE the confirm — `cost_rejected` (422) carrying the port's code verbatim
(`unit_cost_line_missing` / `unit_cost_negative` for the two shape violations). The order stays
draft; nothing fires. A confirm is a commitment; an unknown-cost confirm would corrupt margin
analytics silently — refusing is the only safe default. A NULL cost for an item PROCEEDS: no cost
maintained is honest absence, not failure, and the refusal is not sticky (a retry against a healthy
port succeeds). Refusal typing elsewhere is byte-identical to the pre-snapshot era: wrong tenant,
absent id, and non-draft are all `not_draft` — no existence leak.

Known race, deliberately accepted: a draft line ADDED between the read (1) and the stamp (3) is
not in the unnest table and stays NULL — honest absence, never a WRONG cost. Tightening would need
`FOR UPDATE` line reads inside the tx; the line-edit path's own `FOR UPDATE` already serializes
its writes against the stamp.

### 2. Margin: one canonical expression, computed at read time, NULL is honest absence

`unit_cost` NULL ⇒ `margin` NULL ⇒ `margin_percent` NULL — **never zero**. Zero is a real
zero-margin trade (goods at cost); conflating "no cost maintained" with "sold at cost" corrupts
analytics silently. A NULL stamp is deliberately indistinguishable from a never-stamped line (both
mean unknown; no marker column).

```text
line_margin(line)    = line_amount − unit_cost · quantity        // costed lines only; total basis
margin_percent(line) = round2( line_margin / line_amount × 100 ) // None ⇔ line_amount = 0
```

The expression lives once in `selling_margin.rs` and is mirrored textually in the SQL rollup
(`margin_rollup`); drift between the mirrors makes the rollup contradict the per-line computes,
which is the defect class the mirroring documents. Total basis (`line_amount − cost·qty`, not
per-unit then multiplied) avoids a second rounding step against the persisted 2dp `line_amount`.

**Negatives are legal**: cost > price is a real loss; a free promo reward line (zero price) carries
its full cost as a negative margin by design — the cart total was already adjusted when the reward
line was added. **The rollup covers the costed subset only**, with coverage counters
(`marginLinesCosted` / `marginLinesTotal`) so partial-coverage orders are visible; an uncosted line
neither contributes to the sum nor dilutes the percentage. **Returns are NOT netted** — the margin
is the confirmed-line margin; credit notes live in billing (ADR-006) and netting needs a
return-side watermark selling does not own (registered as a later refinement).

`margin` / `marginPercent` are computed at read time and appear on **no write body** — structurally,
they are not schema fields (the ADR-007 `qty_to_invoice` precedent). Served at
`GET /sales-orders/:id/margin`.

### 3. The expense-reinvoice link: selling holds the association, on faith

`ExpenseReinvoiceLink(order_id, expense_id, amount, state pending|invoiced)`. The expense itself
belongs to backbone-expenses; `expense_id` is taken **on faith** (the `opportunity_id` posture — no
cross-module key, no cargo edge). The host validates that the expense exists, belongs to the same
company, and is postable before calling attach; that obligation is the seam's documented contract.

**A pull model, not events**: no new events, no outbox. The host billing adapter (1) pulls
`list_expense_reinvoices(order)` before building a customer invoice and filters `pending`, (2) adds
lines totaling the pending amounts in its own billing shape, (3) calls
`mark_expense_reinvoice_invoiced` per link after its post acks. A **double mark is a LOUD refusal**
(`invalid_transition`), so a billing retry surfaces instead of silently passing. The double-bill
guard is the partial unique `(order_id, expense_id)` over live rows (a soft-deleted link can be
re-attached); the pending queue is indexed `(company_id, state)`. Attaching to a DRAFT order is
allowed (a quote-era charge estimate before confirm is normal); cancelled refuses.

Routes: `GET+POST /sales-orders/:id/expense-reinvoices`, `POST /expense-reinvoices/:id/mark-invoiced`.

### 4. The delivery-carrier registry: master + order link, nothing more

`DeliveryCarrier(name, active, tracking_url_template?)` — **registry only**: no rates, no labels,
no carrier API surface, and no change to the `DeliveryRequested` envelope (inventory consumes
item/qty only; expanding its contract is out of scope). Retirement is **deactivate-don't-delete**:
orders reference a carrier through an FK, so a hard delete of a referenced carrier is
database-blocked; the flag keeps history readable while retiring the name from the active list.
Duplicate names per company refuse (`duplicate_carrier_name`, mirroring the partial unique index).

`sales_orders.delivery_carrier_id + tracking_ref` are **fulfillment metadata, not frozen money**:
writable on draft AND confirmed orders (tracking typically arrives only after ship), refused only
on `cancelled`. Every path that names a carrier validates it with a company-scoped pre-read — an
unknown or cross-tenant id is a clean `carrier_not_found`, never the FK violation's 500. The
patch semantics distinguish keep / clear / set for both fields (missing = keep, `null` = clear).

Routes: `POST+GET /delivery-carriers`, `PATCH /delivery-carriers/:id`,
`POST /sales-orders/set-delivery`; `CreateSalesOrderBody.deliveryCarrierId` for a create-time
choice, validated before the order transaction opens.

### 5. Migration shape

One migration, `20260825000100_selling_margin_delivery_reinvoice`: guarded DO-block enum creation,
`IF NOT EXISTS` object creation, audit triggers + GIN + partial indexes + per-table RLS on the two
new tables (the module's established posture), CHECK constraints landing `NOT VALID` then
`VALIDATE`d, **no backfill** (existing confirmed orders keep NULL costs — honest absence), and a
down side that is the strict reverse.

## Consequences

- Confirm is now a two-party act (selling + the host's cost source). A host that has no cost source
  passes `NoUnitCostPort` and gets NULL-cost confirms; a host whose source is down gets refusals,
  not corrupted margins.
- The margin view is only as good as the snapshots: analytics consumers must treat NULL as
  "unknown", never zero — the coverage counters exist to make that impossible to miss.
- Billing owns rebilling end-state; selling only ever answers "which expenses does this order
  want rebilled, and which have been billed". If billing and the link ever disagree, the loud
  double-mark refusal is the tripwire.
- The carrier registry is a v1 fence: when rates/labels arrive they arrive as a NEW seam, not as
  creep on this master.
