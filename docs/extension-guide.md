# Extension Guide — backbone-selling

> How to embed and extend `backbone-selling` without forking it or breaking on regeneration.
> This is the module's public contract per `docs/erp/extension-contract.md`. Tier B/C surfaces here
> survive `metaphor schema schema generate --force` (proven: `scripts/regen_roundtrip.sh`).

## The public surface (stable)

**A. Domain events** (`application::service::selling_events`) — the primary extension seam:

| Event | Fires when | Carries |
|-------|-----------|---------|
| `QuotationAccepted` | a quotation is accepted | quotation_id, company_id, customer_id |
| `SalesOrderConfirmed` | an order is confirmed | order_id, company_id, customer_id, grand_total, currency |
| `SalesInvoiceIssued` | an invoice is created | invoice_id, sales_order_id?, company_id, customer_id, total |
| `SalesInvoicePosted` | revenue posts to the GL | invoice_id, company_id, journal_id, post_id, total |

**B. Exported DTOs** — `SalesOrderRef`, `QuotationRef` (`{id, customer_id, company_id, grand_total,
currency}`); build via `SellingWriteService::sales_order_ref`.

**C. The outbound GL port** — `selling_gl::{AccountingPostEnvelope, GlPostLine, GlPostSink, GlPostAck,
GlPostRejected}`. A composing service implements `GlPostSink` (map envelope → your ledger) to receive
revenue postings. The envelope is the versioned wire contract — not a shared Rust type.

## How a consumer extends (the supported pattern)

1. **Subscribe to a domain event.** Implement `SellingEventSink` in your own crate / a `*_custom.rs`
   sibling and pass it to `SellingWriteService::with_sink(pool, my_sink)`. Add your rule in the
   handler. Selling never calls back into you.
2. **Keep your logic in `user_owned` / `*_custom.rs` files.** They are listed in
   `metaphor.codegen.yaml` and skipped wholesale by regen — your rule survives module regeneration.
3. **Never edit generated code** outside `// <<< CUSTOM` markers, and never edit another module's
   schema.

### Reference consumer (in-repo)

`application::service::consumer_credit_rule_custom::CreditWatchConsumer` subscribes to
`SalesOrderConfirmed` and flags over-limit orders — a downstream rule added purely through the event
surface. `tests/extension_contract.rs` drives it; `scripts/regen_roundtrip.sh` proves it survives a
regen of the module (extension-contract §5, second clause).

```rust
let consumer = Arc::new(CreditWatchConsumer::new(dec!(5_000_000)));
let selling  = SellingWriteService::with_sink(pool, consumer.clone());
// ... confirm orders; consumer.breaches() holds the rule's decisions.
```

## Composing the HTTP surface

Mount `presentation::http::create_guarded_selling_routes(&module, pool)` — read + validated create,
no generic mutation. Posting is service/job-driven (needs your `GlPostSink`), not an HTTP route.

## What is NOT a contract

`// <<< CUSTOM` blocks inside generated files (your own edits only, not cross-module extension);
internal repositories/services; the generated CRUD events (`*Created/Updated/Deleted`) — prefer the
semantic domain events above.

## Deferred surfaces (not yet stable)

Inbound projection sync (`ItemCreated`/`PartyCreated` → local read-models), delivery events
(`DeliveryRequested`, `delivered_qty`), and multi-currency — all land with `backbone-inventory` / an
FX contract. Design against the domain events above; these will be additive.
