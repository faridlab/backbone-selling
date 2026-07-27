---
date: 2026-07-27
repo_type: module
unit: backbone-selling
version: 0.5.3
focus: maturity
roster: [chair, skeptic, steelman, yagni-business, ddd-bounded-context, contract-seat, domain-expert]
note: Second maturity pass — a 2026-07-03 completeness council already fixed the 5→7 order-state downgrade; this run does not re-litigate it.
---

# Council — module:backbone-selling — focus: maturity

## Best call
**Fix `mark_delivered` to be the capacity-checked, `FOR UPDATE`-serialized, `OverDelivered`-rejecting twin of `mark_invoiced`** — add an `allocate_delivered` mirroring `allocate_billed` (invoice_seam.rs:94-109), an `OverDelivered` variant beside `OverBilled` (selling_write_service.rs:165), wrap the loop in one tx with `lock_delivery_capacity`, and ship the one delivered-bounds test the skeptic named as the cheap probe. Correctness beats maturity here because this bug does not merely dent maturity — it silently falsifies the headline invariant (`completed iff fully billed AND fully delivered`, ADR-003/004) the entire steelman case rests on; a module whose marquee invariant is quietly false is not mature, it is mature-looking. The contract leak (raw CRUD on the struct) needs active misuse to bite; the over-delivery hole bites on a single off-by-one inbound event with no error, no dead-state, no audit trail.
- Residual negative value: ~half a day of work (the billing mirror is the template — copy, rename, invert the guard). One new error path the inbound composition must now handle: a previously-silent `mark_delivered` will return `OverDelivered`, so the inventory ACL / `StockDelivered` relay needs a rejection policy (DLQ vs. partial-accept) — that is the real carry, not the lock. Adds one symmetric `lock_delivery_capacity` `FOR UPDATE` query on `sales_order_items` (same shape as billing, no new coupling).
- Reversibility: easy. Additive guard + error variant; revert is mechanical and breaks no caller (the Ok path is unchanged).
- What would flip this: a DB-level `CHECK (delivered_qty <= quantity)` on `selling.sales_order_items`, OR proof that inventory's `StockDelivered` is provably capped at the order line. Neither exists; the only test drives the exact-quantity golden case.

## Disagreement map
1. **Correctness vs maturity as the headline move.** Skeptic + Domain-Expert say the over-delivery hole is the move; Contract + YAGNI say the struct leak and dead ES scaffold are the maturity story. Crux: a silent invariant-falsification is not a maturity ding, it is a maturity-claim falsifier — so it outranks a bypass that requires active misuse. Chair sides with Skeptic+Domain-Expert; the maturity scorecard below reflects the downgrade.
2. **Mechanism of the fix — lock vs cap.** Domain-Expert frames it as "give `mark_delivered` FOR UPDATE"; Skeptic correctly clarifies that the lock alone does NOT close it — the rollup's `>=` trusts an upstream cap that does not exist for delivery, so the cap check (reject `OverDelivered`) is the load-bearing half. Chair sides with Skeptic on the mechanism: the fix is `allocate_delivered` (cap + reject), with the lock as serialization, not the reverse.
3. **Maturity verdict.** Steelman claims 5/5 (most mature module); Contract (3/5), DDD (4/5), Domain-Expert (4/5) all sit below. Crux: the steelman's case is conditional on invariants the skeptic just showed are silently false — so the unqualified "most mature" claim does not survive this pass.

## Recommendations (ranked by leverage)
| # | Move | Leverage | Residual negative | Reversibility | Evidence to flip |
|---|------|----------|-------------------|---------------|------------------|
| 1 | **Capacity-check `mark_delivered`** (`allocate_delivered` + `OverDelivered` + tx) and ship the `qty = line_qty + 1` bounds test | Closes a silent correctness hole that falsifies ADR-003/004; makes the rollup's `>=` trustworthy for delivery | ~0.5 day; one new rejection path for the inbound ACL to policy | easy | DB `CHECK` constraint capping `delivered_qty`, or proof inventory caps `StockDelivered` |
| 2 | **Promote `SellingWriteService` into the `SellingModule` builder; demote raw CRUD to a `crud_services()` accessor** (Contract seat) | Closes the invariant-bypass surface (`.create()`/`.update()`/`.delete()` on `module.sales_order_service`); makes ADR-001 §2 ("generic mutation must NOT be mounted") true at the struct, not just the router | Touches every consumer of the struct fields; one release-cycle deprecation | costly (API break) | Evidence no consumer reaches the raw handles today |
| 3 | **Retire `post_sales_invoice` / `create_invoice_from_order`** per ADR-005 parking lot (DDD seat) | Removes the "dead-in-composed-flow" legacy path and the Invoice double-meaning; the test at delivery_seam.rs:187-188 still calls it, so it must move to the billing composition | Test rewrite — the golden path currently exercises the legacy post | easy (deletion) | Evidence a live caller still mounts the legacy path |
| 4 | **`#[cfg]`-gate or drop the unused ES scaffold** (`event_store/`, `snapshot_store.rs`, `subscriptions/`) (YAGNI seat) | Removes framework-pin-bump churn (v2.7.5→2.7.6 visible in commits) and the false "selling is event-sourced" signal | If a P1 outbox rollout needs it, re-adding is cheap (the seams are load-bearing, ES is not) | easy | A live writer that goes through `PostgresEventStore` instead of `SellingRepos` |

## Maturity scorecard
- **ddd-bounded-context — 4/5.** Vocabulary is consistent and the 7-state model is restored, but the folded-in Sales Invoice is a carried, ADR'd-but-dead-in-composed-flow boundary violation (`post_sales_invoice`/`create_invoice_from_order`); language consistent, one zombie at the boundary.
- **contract-seat — 3/5.** The real contract (`SellingWriteService`) is constructed inline at `guarded_routes.rs:257`, not in the builder; 8 raw `GenericCrudService` handles are the headline struct surface and are bypassable, contradicting ADR-001 §2 at the struct layer.
- **domain-expert — 3/5.** `mark_invoiced` is exemplary, but `mark_delivered` is uncapped and the rollup's `>=` trusts a cap that does not exist for delivery — the model does NOT represent the no-over-delivery rule, so "every real state/rule" fails on a headline invariant.

## Parking lot
- **Credit-hold state** — raised by Domain-Expert, scope: root (model-state gap; the model can flag a breach via a consumer but has no state to gate on).
- **Cross-module FKs are `@exclude_from_foreign_key_check` logical refs with no DB guarantee** — raised by Contract (secondary), scope: other (cross-module referential integrity, not selling-local).
- **Envelope scope: IDR-only / single-tax / no-partial-delivery** — raised by Steelman (condition c), scope: root (envelope schema scope expansion).
- **Consuming ledger's partial unique index is the real `posting_state` guard** — raised by Steelman (condition b), scope: other (downstream ledger invariant, not selling-local).
