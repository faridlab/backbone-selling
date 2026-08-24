# ADR-007: The invoicing-policy engine, the quotation machine, and the order's one-way exits

**Status**: Accepted — **Applied 2026-08-24**
**Related**: [ADR-003](ADR-003-order-status-model.md) (the watermark rollup this engine now feeds), [ADR-005](ADR-005-invoice-seam.md) (the billing-owned invoice seam that consumes it), [ADR-006](ADR-006-selling-exits-invoice-business.md) (why selling computes invoiceability instead of invoicing)

## Context

Odoo's `sale.order` carries two features selling lacked: per-line **invoicing policy** (bill on
confirmation vs bill on delivery) and a full **quotation state machine** (send / re-draft /
reject / cancel). Selling already had the two watermarks (`billed_qty`, `delivered_qty`) and a
status rollup (ADR-003), but both compared against the **ordered quantity** unconditionally. Under
a delivery policy that comparison strands the order: a line delivered 6 of 10 and billed for those
6 satisfies the business ("bill what you shipped") yet the rollup still reads `billed_qty <
quantity` and the order sits in `to_deliver_and_bill` forever.

The quotation side had only `accept` / `convert`; a sent quotation could not be rejected or
withdrawn, and a cancelled one was dead rather than re-editable. And an order once confirmed had
no exit at all — no cancel verb, no line-edit discipline — so a mistaken order could only be
soft-deleted, which orphans the downstream billing/delivery intent without a trace.

## Decision

### 1. The invoicing policy is ONE canonical expression, consumed by every site

A line's `invoice_policy` (`order` — the default, or `delivery`) decides when its quantity becomes
invoiceable. The basis is a single expression, mirrored textually in the four places that need it:

```text
policy_base(line) = (invoice_policy == 'delivery' AND NOT is_downpayment) ? delivered_qty : quantity
qty_to_invoice    = policy_base − billed_qty        // raw; negative on upselling/returns
```

- `selling_invoice_policy.rs` (Rust mirror + the read models),
- `list_billing_remainders` (what `build_invoice_request` asks billing for),
- `lock_billing_capacity` (the `FOR UPDATE` bound that caps `mark_invoiced`),
- `watermark_rollup` (the ADR-003 status recompute).

Drift between these mirrors is the defect class this ADR exists to prevent: any site comparing
against a different basis either strands orders or over/under-requests invoices. The unit goldens
(`tests/invoice_policy_compute.rs`) prove the mirrors agree end-to-end.

`qty_to_invoice` and `invoice_status` are **computed at read time and never persisted** — no write
route accepts them, structurally (they are not schema fields). The read surface is
`GET /sales-orders/:id/invoice-status` (+ the quotation mirror), serving per-line computes and the
order aggregate.

### 2. Downpayment lines: quantity basis, excluded from aggregates

`is_downpayment` marks an advance. Billing's downpayment advances precede delivery, so such a line
always stays on the **quantity** basis even under a delivery policy — but it is excluded from the
order-level aggregate AND from both bands of the watermark rollup (a downpayment's placeholder
quantity is never delivered; counting it would strand the delivered band the same way the
pre-policy rollup stranded the billed band).

### 3. Line status vocabulary (Odoo's, two deliberate deltas)

Per line: `no` | `to invoice` | `invoiced` | `upselling`.

- `upselling` = billed exceeds the ordered quantity on the ordered basis.
- **Delta 1 — "invoiced" requires billed > 0.** A delivery-policy line with zero delivery and zero
  billing reads `no`, not `invoiced`: "invoiced" claims work that was never done. (Odoo's numeric
  fallthrough reports a delivered-nothing line as invoiced on some policy mixes.)
- **Delta 2 — the order aggregate is actionable-first.** Any `to invoice` line outranks any
  `upselling` line. Odoo's loop lets a late upselling line overwrite `to invoice`; ours answers
  "what do I do next", so the actionable state wins.

### 4. The quotation machine (Odoo `sale.order` semantics, adapted)

| verb | from | to | refused with |
|------|------|----|--------------|
| send | draft | sent | `invalid_transition` |
| accept | draft, sent | accepted | `not_draft` (existing) |
| reject | sent | rejected | `invalid_transition` |
| cancel | draft, sent, accepted | cancelled | `invalid_transition`; from `ordered` → `quotation_ordered` |
| re-draft | sent, rejected, cancelled | draft | `invalid_transition` (never from `ordered`) |

Every verb is a guarded single-statement flip whose `WHERE` clause IS the guard: a wrong-state or
wrong-tenant id is refused without leaking whether the id exists. The precise error code comes from
a classification read that runs **only after** a refusal. `ordered` is a one-way door — a confirmed
order must never be orphaned by resetting or cancelling its source quotation. The optional
reject/cancel reason persists as `status_reason` (cleared on re-draft).

Refusals are LOUD 422s. This is the module's standing answer to Odoo's `_create_invoices`
silent-return class of defect: never return nothing when the caller asked for something.

### 5. The order's one-way exits

- **Cancel** (`POST /sales-orders/cancel`): draft/to_deliver/to_bill/to_deliver_and_bill →
  cancelled. Refused with `order_billed` when any live line carries `billed_qty > 0` — posted
  invoices are never cancelled; a credit note is the correction path. The billed check and the flip
  are ONE atomic statement, so a racing `mark_invoiced` cannot slip a billed quantity between check
  and flip. A **delivered-but-unbilled** order CAN be cancelled (only billed guards; delivery
  reversal is inventory's lane). A terminal order (completed/closed) refuses with
  `invalid_transition`. Emits `SalesOrderCancelled`.
- **Line freeze** (`PATCH /sales-orders/lines/:id`): once the status has left `draft`, the item /
  quantity / price / discount fields refuse with `order_line_frozen` — only the description stays
  editable (the label, not the commitment). On a draft, a priced edit re-prices the line and
  re-derives the header's subtotal/tax/total from the live line set in the same transaction. The
  line + its parent header are locked `FOR UPDATE` together, so the freeze check cannot race a
  concurrent confirm.

### 6. Quotation templates (fenced master data)

`QuotationTemplate` (name, validity_days, default_notes; unique `(company_id, name)` among live
rows, RLS-fenced like every selling table). A quotation create that passes `template_id` stamps
`valid_until = quotation_date + validity_days` and the default notes **only where the caller
supplied none** — the caller's values always win. The template itself is not persisted on the
quotation; its effects are stamped at create.

### 7. The opportunity link

`Quotation.opportunity_id` is a logical M2O to the deal module's Opportunity: the host passes it,
selling stores it, no Cargo edge and no cross-module key. It stays optional on the create input so
a host that has not migrated keeps compiling.

## Consequences

- Delivery-policy orders no longer strand: the delivered 6 / billed 6 / ordered 10 line reads
  `invoiced` (billed for everything deliverable so far), the order advances to `to_deliver`, and
  the line reopens as `to invoice` when more delivery lands.
- The billing seam (`build_invoice_request` / `mark_invoiced`) now honors the policy with zero
  change to its own shape — the basis lives inside selling's SQL, which was the point of keeping
  the expression single-sourced.
- `status_reason` + the machine verbs give the CRM host a real quotation lifecycle to subscribe to
  (`QuotationSent` / `QuotationRejected` / `QuotationCancelled` / `QuotationReDrafted` /
  `SalesOrderCancelled` events).
- The read DTOs for the invoice-status endpoints live in the user-owned
  `selling_invoice_policy.rs`, not the generated `presentation/dto/*.rs`: a forced regen rewrites
  the generated files wholesale and eats hand-appended blocks (observed live, including
  end-of-file CUSTOM markers). Regens also re-emit an incomplete auto-named
  `create_quotation_template_table` migration dated at generation time — delete it on every regen;
  `20260824000100_selling_invoice_policy_and_templates` is the single complete owner of the table,
  enum, ALTERs, and fence.

## Verification

- `tests/quotation_machine.rs` (11): the machine's happy paths, every refusal, the `ordered`
  one-way door, event emissions, cross-tenant 404s.
- `tests/invoice_policy_compute.rs` (23): the policy compute per basis, the stranding fix, the
  watermark bound, downpayment exclusion + completion, aggregate precedence, upselling, draft/closed
  reads, the quotation read model, template goldens, the opportunity link, the line freeze, the
  cancel guard, conversion carrying the flags.
- `tests/integrity_probes.rs` (10, +4): the template routes, the invoice-status route, the
  line-freeze route, and machine-verb tenant scoping.
- Full suite green on a fresh-database chain (all migrations applied in order to a pristine DB)
  and on the standing multi-module test database.
