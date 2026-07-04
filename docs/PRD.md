# PRD — backbone-selling

> Product Requirements. Tier 2 · Supply Chain pillar · Indonesia-first ERP suite. Status: built (proving-ground). Date: 2026-07-03.

## 1. Problem & intent

Businesses selling to customers need a **demand-to-revenue pipeline**: offer a price, confirm the
commitment, bill it, and recognise revenue in the books — without every consuming app (POS,
e-commerce, field sales) re-implementing pricing, document integrity, and GL posting. `backbone-selling`
owns that pipeline as an independent, embeddable module, and is the **reference implementation of the
GL-posting extension contract** for the whole suite.

## 2. Goals

- Own the canonical **order-to-cash** documents: Quotation → Sales Order → Sales Invoice (+ lines).
- Compute money **server-side** (line amounts, totals, tax) so no caller can persist an inconsistent
  document.
- Recognise revenue by emitting a **balanced `AccountingPost`** into `backbone-accounting`
  (Dr A/R · Cr Revenue · Cr PPN Output) over a versioned envelope + ACL — zero horizontal coupling.
- Expose a **stable extension surface** (domain events + `*_custom.rs` + `user_owned` files) so a
  consumer extends behavior (credit rules, fulfillment, notifications) without forking or breaking on
  regen.
- Track **billing and delivery watermarks** (`billed_qty`, `delivered_qty`) and drive the order
  lifecycle to `completed` (fully billed **and** fully delivered).
- Request fulfillment across the **selling↔inventory delivery seam** — emit a `DeliveryRequestEnvelope`
  and record `StockDelivered` via `mark_delivered`, with zero normal Cargo edge on inventory (ADR-004).

## 3. Non-goals (this phase)

- Physical stock movement and COGS posting → `backbone-inventory` (selling only requests delivery and
  records its result; the SLE/Bin and the COGS journal are inventory's — ADR-004).
- Multi-currency revenue posting → guarded IDR-only until an FX contract exists (ADR-002).
- Multi-rate / effective-dated tax computation → `backbone-tax` (selling takes a supplied rate).
- Credit-limit enforcement, product bundles, installation notes → Tier 3 / consumer-side.

## 4. Personas

- **Sales user** — issues quotations, confirms orders, raises invoices.
- **Finance user** — relies on correct, balanced revenue postings and A/R subledger by customer.
- **Integrating engineer (consumer)** — embeds selling in a service and extends it via events +
  `*_custom.rs`; must survive module upgrades/regens.

## 5. Success criteria

- Every document is internally consistent (totals = Σ lines; envelope balances by construction).
- Revenue posting is proven end-to-end against the real ledger, idempotent, concurrency-safe.
- A consumer's custom rule on a selling domain event **survives a regeneration of both modules**
  (extension-contract §5 — proven by `scripts/regen_roundtrip.sh`).
- Indonesia-first: IDR-only guard, PPN Output line, customer as A/R subledger party.

## 6. Scope summary

Owned: Quotation, SalesOrder(+items), SalesInvoice(+items), SalesTeam/SalesPersonAllocation.
Emitted events: `QuotationAccepted`, `SalesOrderConfirmed`, `SalesInvoiceIssued`, `SalesInvoicePosted`,
`DeliveryRequested`. Logical FKs (no DB constraint): customer→party, item→catalog,
company/branch→organization, GL accounts→accounting. Deferred: COGS posting (inventory's), bundles,
credit-limit, multi-currency, real tax.
