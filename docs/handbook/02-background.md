<!-- Reader: Evaluator · Mode: Explanation -->
# Background & prior art

`backbone-selling` sits at the intersection of two lineages: **ERP selling modules** (which define
what an order-to-cash domain *is*) and **schema-first codegen** (which defines how the code is
*built*). It borrows deliberately from both and rejects specific things about each. This page credits
the prior art honestly and says exactly what selling keeps and drops.

## Lineage 1 — ERP selling / order-to-cash modules

Selling did not invent the Quotation → Order → Invoice pipeline. Every serious ERP has it.

### SAP Sales & Distribution (SD)

The reference implementation of order-to-cash: inquiry → quotation → sales order → delivery →
billing, with pricing, credit management, and a tight coupling to FI (financials) so billing posts to
the GL.

- **What's good:** the document-flow model (each document references its predecessor) and the hard
  rule that *billing is the revenue event*, not the order.
- **Where it hurts:** SD is monolithic and deeply coupled to the rest of SAP — you cannot lift the
  selling logic out without FI, MM, and the customer master coming with it.
- **Selling keeps:** the document flow (`quotation_id` on the order, `sales_order_id` on the invoice)
  and "the invoice is the revenue event."
- **Selling rejects:** the coupling. Its customer, item, and account references are *logical* FKs, and
  it posts to accounting through a serialized envelope, not a shared table.

### Odoo Sales / ERPNext Selling (open-source ERP)

Both model a `Sales Order` with lines, convert quotations, and hand off to an invoicing module that
posts a journal entry. ERPNext's Selling is the closest analogue: Quotation, Sales Order, Sales
Invoice, plus a Sales Team / commission split — the same entity set selling ships.

- **What's good:** pragmatic scope; the Sales Team / commission-allocation concept
  (selling's `SalesTeam` + `SalesPersonAllocation` come straight from this lineage).
- **Where it hurts:** in a shared-database monolith, the selling code and the accounting code run in
  one process against one schema and call each other directly — independence is a convention, not an
  enforced boundary.
- **Selling keeps:** the entity set and the commission-split model.
- **Selling rejects:** the direct call into accounting. The boundary is enforced by the compiler
  (zero horizontal Cargo edges), not by discipline.

### The order-status insight

ERP selling modules encode **delivery and billing progress as order states** (ERPNext's
"To Deliver and Bill" / "To Bill" / "Completed"). Selling's first build collapsed this to a naïve
5-state model and lost the intent; the completeness council restored the brief's **7-state** model.
That history is recorded in [ADR-003](../adr/ADR-003-order-status-model.md) — the states are not
cosmetic, they *are* the delivery/billing watermarks.

## Lineage 2 — how the code is built (codegen prior art)

The second lineage is about generating the layer cake instead of writing it. This is the framework's
inheritance, shared by every Backbone module; selling is one instance of it.

| Approach | Example | Borrowed | Rejected |
|----------|---------|----------|----------|
| **Hand-rolled layers** | writing entity + DTO + migration + repo + handler by hand | the explicit, readable 4-layer DDD structure — you can still step through every generated file | writing the mechanical 95% by hand; it drifts entity-to-entity |
| **Heavyweight ORMs** | Rails, Django, Hibernate | the leverage — generic CRUD you inherit, not write | runtime magic and fat-model domain/DB coupling; selling generates *visible* Rust and keeps SQLx **compile-time-checked** |
| **Schema-first codegen** | Prisma, OpenAPI, protobuf | one source of truth + full-artifact generation | the all-or-nothing edit boundary; `// <<< CUSTOM` + `user_owned` let generated and hand-written code coexist ([ADR-0003](adr/adr-0003-custom-markers.md)) |
| **Scaffolders** | Laravel `make:*` | the ergonomic entry point (`metaphor make entity`) | the one-shot nature; Backbone generation is idempotent and repeatable for the life of the module |

The synthesis: **repeatable, compile-time-checked, regen-safe scaffolding over a strict DDD skeleton.**
ORM-level leverage with hand-rolled-level transparency, regenerable forever without losing custom
logic — see [Philosophy](01-philosophy.md).

## Lineage 3 — how modules talk to the ledger (the seam prior art)

The hardest question selling answers is not "how do I model an invoice" but "how does a transactional
module post to the GL without depending on it." Three known patterns, and what selling chose:

1. **Direct call / shared database (the monolith default).** Selling code calls the accounting
   service or `INSERT`s a journal row. Simple, but couples the two and lets anyone unbalance the
   books. **Rejected.**
2. **Fire-and-forget event bus.** Selling publishes "invoice submitted"; accounting subscribes and
   posts asynchronously. Decoupled, but the producer never learns whether the post balanced or was
   rejected, and idempotency is the subscriber's problem. **Partially adopted** — selling *does* emit
   domain events for extension (below), but revenue posting is not fire-and-forget.
3. **Serialized envelope through an anti-corruption layer (selling's choice).** Selling builds a
   balanced `AccountingPostEnvelope`, hands it to a `GlPostSink` trait, and the composing service maps
   it onto accounting's real `PostingService`. Synchronous, idempotent (`source_id = invoice_id`),
   and the envelope is the versioned contract. Selling never names an accounting type; accounting
   never imports selling. This is the [GL-posting seam](../adr/ADR-002-gl-posting-seam.md), and it is
   the reference implementation the next producer (Inventory COGS, split-out Billing) reuses verbatim.

And a fourth, orthogonal pattern for *non-ledger* extension: selling **emits domain events**
(`SalesOrderConfirmed`, …) that arbitrary consumers subscribe to — the credit-watch example in
[`tests/extension_contract.rs`](../../tests/extension_contract.rs) decides a credit limit off
`SalesOrderConfirmed` without selling knowing the consumer exists. Selling emits; the consumer
decides.

## Where selling sits in the Metaphor workspace

Selling is one project type among several the [Metaphor CLI](../schema/INTEGRATION.md) orchestrates:

- **`crate`** — a focused Rust library.
- **`module`** — *this* — a bounded domain library (4-layer DDD, schema-driven), **consumed by
  services, never run alone.** Selling is the **first Tier-2 GL producer** built in the workspace.
- **`backend-service`** — a runnable Axum/SQLx/Tonic server that *composes* modules and supplies the
  `GlPostSink` that bridges selling to accounting.

Selling borrows identity from `sapiens` (`User`, for audit actors and sales people) by logical
reference, references `party` / `catalog` / `organization` / `accounting` the same way, and is wired
into a service by that service's composition root. The [Architecture](04-architecture.md) page shows
exactly where the seams are.

---

Next: [Technology & the "why"](03-technology.md) — the stack, choice by choice, including the ones
selling leans on hardest (decimal money, serde-as-contract).
