<!-- Reader: Evaluator · Mode: Explanation -->
# Philosophy & motivation

**`backbone-selling` is the order-to-cash intent of an ERP, expressed as one bounded module that
*emits* accounting instead of *owning* it.** It runs three documents down one pipeline —
Quotation → Sales Order → Sales Invoice — and at exactly one point, when an invoice is submitted, it
posts a balanced revenue entry into the General Ledger. It does that without importing the ledger,
without holding a single master record, and without letting a client hand it a number it did not
compute itself.

Two convictions sit underneath that sentence: one about *how the code is built* (it is generated
from a schema, not written), and one about *what this domain is allowed to do* (it produces GL
postings; it is never the ledger). This page explains both, and is honest about what selling
deliberately refuses to do.

## The problem — order-to-cash is where modules usually bleed into each other

Every ERP has the same pipeline: a customer is quoted, the quote becomes an order, the order is
delivered and billed, and the bill recognises revenue in the ledger. The naïve implementation
couples all of it: the selling code reaches into the customer master, the item catalog, and the
accounting tables, joins across them, and writes journal rows directly. The moment it does, three
things break:

- **Independence is gone.** Selling can't be deployed, tested, or reasoned about without accounting,
  parties, and catalog all present and consistent.
- **The ledger loses its authority.** If any module can `INSERT` a journal row, "the books balance"
  is a hope, not an invariant.
- **The boilerplate buries the domain.** Each of selling's eight entities re-litigates the same
  CRUD layer cake — struct, DTOs, migration, repository, service, handler, pagination, soft-delete,
  error shape — so the 5% that is *actual selling logic* (how a quotation becomes an order, how an
  invoice recognises revenue) drowns in the 95% that is mechanical.

Selling is built to make all three impossible.

## Conviction 1 — the plumbing is generated, not written

A Backbone module describes *what* each entity is in one YAML file; the framework generates the
entity struct, DTOs, migration, repository, service, HTTP handler, and routes — twelve REST
endpoints per entity — from that description. You write only the logic that is genuinely selling's.

Three framework rules make that safe:

1. **The schema is the single source of truth.** [`schema/models/<entity>.model.yaml`](../schema/RULE_FORMAT_MODELS.md)
   is authoritative. Selling's eight entities — `Quotation`, `QuotationItem`, `SalesOrder`,
   `SalesOrderItem`, `SalesInvoice`, `SalesInvoiceItem`, `SalesTeam`, `SalesPersonAllocation` — and
   every DTO, migration, and repository are *downstream artifacts*. If code and schema disagree, the
   schema is right and the code is stale. (See [ADR-0001](adr/adr-0001-schema-yaml-ssot.md).)

2. **Boilerplate is generic, so it is inherited once.** `QuotationService` is a **type alias** over
   `GenericCrudService`, not an `impl`; `QuotationRepository` is a **newtype** over
   `GenericCrudRepository`. Every entity gets the *same* soft-delete, pagination, and error shape
   because they all come from the same generic code. (See [ADR-0002](adr/adr-0002-generic-crud.md).)

3. **Hand-written code survives regeneration.** Selling's real logic — the validated write path, the
   GL-posting seam, the domain events — lives where the generator never overwrites it: `// <<< CUSTOM
   … // END CUSTOM` markers and whole files listed under `user_owned` in
   [`metaphor.codegen.yaml`](../../metaphor.codegen.yaml). You can regenerate forever without losing
   it. (See [ADR-0003](adr/adr-0003-custom-markers.md).)

The payoff for selling specifically: the eight CRUD surfaces cost one YAML file each, and the whole
of the engineering attention goes to the three things that are *actually* the selling domain — the
pipeline, the money, and the GL seam.

## Conviction 2 — selling produces accounting; it is never the ledger

This is the philosophy that makes selling *selling* and not a generic CRUD app. It comes from
[ADR-001](../adr/ADR-001-selling-boundary.md) and [ADR-002](../adr/ADR-002-gl-posting-seam.md), and
it has four load-bearing parts.

### Three documents, one pipeline — only the invoice touches the ledger

`Quotation → SalesOrder → SalesInvoice`, each with line children.

- A **Quotation** is a priced, time-boxed *offer*. It carries intent and totals; it posts nothing.
- A **SalesOrder** is *confirmed demand*. Created directly or converted from an accepted quotation.
  It owns intent only — no GL post.
- A **SalesInvoice** is the one document that **recognises revenue**. On submit, it emits a balanced
  entry to the ledger: `Dr Accounts-Receivable · Cr Revenue (per income account) · Cr PPN Output`.

Order↔Quotation and Invoice↔Order links are intra-module foreign keys; everything cross-module is a
*logical* reference (below). One document recognises revenue, and it is always the same one.

### Selling holds no masters — zero horizontal edges

Selling owns no customers, no items, no accounts, no companies. Each is a **logical foreign key**
(`@exclude_from_foreign_key_check`) to the module that *does* own it: `customer_id → party.Party`,
`item_id → catalog.Item`, `company_id → organization.Company`, and the GL account references
(`receivable_account_id`, `revenue_account_id`, `tax_output_account_id`) → `accounting.Account`.
None is a database constraint into another schema, so selling has **zero horizontal Cargo edges** and
never joins across a module boundary. `cargo tree -e normal -i backbone-accounting` on this crate is
empty — a fact the seam test enforces, not a hope.

### It emits a posting; it never writes a journal row

When an invoice is submitted, selling builds a `Serialize`-able `AccountingPostEnvelope` — a balanced
posting *request* — and hands it to a `GlPostSink` trait. The composing service implements that sink
over accounting's real `PostingService` (an anti-corruption layer). Selling never names an accounting
type; the **envelope is the versioned contract**, not a shared struct. The builder refuses to emit an
unbalanced envelope (`Σ debit == Σ credit == total`), so the ledger's balance check is never the
first line of defense. This is the marquee decomposition claim — *transactional modules stay
independent of the GL because they emit a request instead of calling into it* — proven end-to-end in
[`tests/gl_posting_seam.rs`](../../tests/gl_posting_seam.rs), not asserted.

### Money is computed server-side; document state and posting state are separate

The client never supplies a total. The **guarded write surface**
([`create_guarded_selling_routes`](../../src/presentation/http/guarded_routes.rs)) mounts read
endpoints plus *validated creates* only — the generic create/update/delete CRUD is **not** exposed —
so a caller cannot post an invoice with an inconsistent `total`, no lines, or a server-owned field it
should not set. The write path computes `line_amount = money(qty·price) − discount` and the totals
(`subtotal / tax_amount / total`) at 2 decimals, round-half-up. And two lifecycles move
independently: `status` (draft/submitted/paid…) is the *document* state; `posting_state`
(pending/posted/failed) is the *GL reconciliation* state, set from the posting acknowledgement.

## What selling deliberately does **not** do

Non-goals are why the boundary stays clean. These are declared deferrals (the ADR "parking lot"),
not oversights.

- **It is not a service.** Selling is a **library crate** — `[lib]` only, no `main.rs`. A
  `backend-service` composes it, hands it a Postgres pool and a `GlPostSink`, and mounts its router.
- **It does not own the physical move or COGS.** The stock movement and the cost-of-goods-sold post
  belong to `backbone-inventory`. Selling only *requests* delivery and *records* its result: it emits
  a `DeliveryRequestEnvelope`, and an inbound `mark_delivered` advances the `delivered_qty` watermark
  when inventory reports a `StockDelivered` — zero normal Cargo edge on inventory. As of 2026-07-04
  the full seam is live and proven end-to-end (`to_deliver` / `to_deliver_and_bill` are reached now,
  and `completed` requires fully billed **and** fully delivered), so the once-dark delivery band is
  now exercised ([ADR-003](../adr/ADR-003-order-status-model.md), [ADR-004](../adr/ADR-004-delivery-seam.md)).
- **It does not compute real tax.** The invoice carries a single supplied `tax_rate` and computes one
  PPN amount. Multi-rate, effective-dated, inclusive tax is `backbone-tax`'s job; wiring it in is
  future work over the same envelope pattern.
- **It does not enforce credit limits, apply payments, or bundle products.** Credit-limit checking is
  a *consumer's* decision made off selling's `SalesOrderConfirmed` event (the extension contract, not
  selling's core); payment application against `outstanding_amount` and product bundles are parked.
- **It does not do multi-currency.** Revenue posts are **IDR-only** until FX is designed — enforced
  both by `build_revenue_post` (rejects non-IDR, `unsupported_currency` 422) and a `CHECK (currency =
  'IDR')` on `selling.sales_invoices`, so a foreign invoice can never silently book face value into
  an IDR ledger.

## When this is the wrong lens

Be honest before you borrow this design:

- If your domain is **not document-and-pipeline shaped** — a pure calculator, a stream processor — the
  three-document / GL-producer structure buys you nothing.
- If you need to **own the ledger**, you want `backbone-accounting`, not a producer like selling.
- If you are **not on PostgreSQL**, the migration and repository generators target Postgres
  specifically.

For an order-to-cash domain that must recognise revenue *without* swallowing the ledger — and for any
future producer (Buying → Inventory COGS, a split-out Billing) that needs the same seam — selling is
the reference implementation.

---

Next: [Background & prior art](02-background.md) — the ERP lineage and the codegen lineage, and what
selling borrows from each.
