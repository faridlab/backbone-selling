<!-- Reader: App developer · Mode: Tutorial → How-to -->
# Developer Guide

Integrate `backbone-selling` into a service, drive the order-to-cash pipeline, and post revenue to
the ledger. The tutorial part holds your hand once through a quotation-to-posted-invoice run; the
recipes assume you know your way around.

Commands here were run against `metaphor 0.2.0`. Where the skeleton [README](../../README.md) shows a
`backbone-schema`/`backbone` command, use the `metaphor` form below — those work today.

## Prerequisites

- **Rust** (2021 edition toolchain) and **Cargo**.
- The **`metaphor`** CLI on your `PATH` (`metaphor --version` → `metaphor 0.2.0` or newer).
- A reachable **PostgreSQL** instance.

## Install — add selling to a service

Selling is a library crate; you consume it from a `backend-service`. Add it as a dependency (git or
path) and wire it in:

```toml
# in the service's Cargo.toml
backbone-selling = { git = "https://github.com/faridlab/…", branch = "main" }
```

```rust
// in the service's composition root
let selling = backbone_selling::SellingModule::builder()
    .with_database(pool.clone())
    .build()?;

// RECOMMENDED surface: read all documents + validated creates. Generic mutation is NOT mounted.
let selling_routes = backbone_selling::create_guarded_selling_routes(&selling, pool.clone());
let app = axum::Router::new().nest("/api/v1", selling_routes);
```

That is the whole integration. `SellingModule` needs only a Postgres pool; the GL-posting seam needs
a `GlPostSink` you supply (see the recipe below), but the HTTP surface does not.

> **Pick the right surface.** `create_guarded_selling_routes` is the production surface — validated
> writes, server-computed totals. `SellingModule::all_crud_routes()` mounts the full *unguarded*
> 12-endpoint CRUD on every entity (trusted/admin/seeding only); `SellingModule::routes()` is a
> `#[deprecated]` alias for it. When in doubt, use the guarded routes.

## Quickstart — prove the toolchain end to end

Point at a database and run selling's own oracle. This proves the schema, migrations, and the golden
cases without writing a line of glue.

```bash
export DATABASE_URL="postgresql://root:password@localhost:5432/skeletondb"

metaphor schema schema validate      # 1. schema is well-formed
metaphor migration run               # 2. apply selling.* migrations (enums, 8 tables, triggers, IDR check)
metaphor dev test                    # 3. run unit + golden + seam + probe tests
```

Expected: validation passes; migrations create the `selling` schema and its eight tables; the test
run exercises the golden cases (`selling_golden_cases.rs`), the guarded-surface probes
(`integrity_probes.rs`), and the order-to-cash conversion (`order_to_cash.rs`). The GL-posting seam
test (`gl_posting_seam.rs`) drives the **real** accounting ledger and needs both `selling.*` and
`accounting.*` schemas in the database.

## Tutorial — a quotation to a posted invoice

Assuming your service mounts `create_guarded_selling_routes` at the root (nest under `/api/v1` if
your service does — adjust the paths). All request/response JSON is **camelCase**.

```bash
# 1. Create a quotation (totals are computed for you — do NOT send subtotal/total).
curl -s -X POST localhost:8080/quotations -H 'content-type: application/json' -d '{
  "quotationNumber": "QUO-2026-00042",
  "companyId": "<company-uuid>",
  "customerId": "<customer-uuid>",
  "quotationDate": "2026-07-01",
  "taxRate": "11",
  "lines": [
    { "itemId": "<item-uuid>", "quantity": "10", "unitPrice": "100000", "lineDiscount": "0" }
  ]
}'
# → 201 { "id": "<quotation-uuid>" }   (subtotal 1,000,000 · tax 110,000 · total 1,110,000, server-computed)

# 2. Create a sales invoice that recognises revenue. Each line needs a revenue account;
#    the header needs an A/R account, and a PPN-output account iff taxRate > 0.
curl -s -X POST localhost:8080/sales-invoices -H 'content-type: application/json' -d '{
  "invoiceNumber": "INV-2026-00042",
  "companyId": "<company-uuid>",
  "customerId": "<customer-uuid>",
  "invoiceDate": "2026-07-01",
  "taxRate": "11",
  "receivableAccountId": "<ar-account-uuid>",
  "taxOutputAccountId": "<ppn-output-account-uuid>",
  "lines": [
    { "itemId": "<item-uuid>", "revenueAccountId": "<revenue-account-uuid>",
      "quantity": "10", "unitPrice": "100000" }
  ]
}'
# → 201 { "id": "<invoice-uuid>" }   (invoice is draft/pending — not yet posted)
```

The invoice exists in `draft` with `posting_state = pending`. **Posting is not an HTTP route** — it
needs a `GlPostSink`, so it runs in the service layer (or a posting job):

```rust
// svc-side: post the draft invoice's revenue into the ledger
let write = backbone_selling::SellingWriteService::new(pool.clone());
let outcome = write.post_sales_invoice(invoice_id, &my_gl_sink).await?;
// invoice → posting_state=posted, status=submitted, journal_id + accounting_post_id set,
// outstanding_amount = total; a re-post is idempotent (one journal, ever).
```

## Key concepts

Five ideas carry you the rest of the way. One line each; the linked page explains *why*.

- **Three documents, one pipeline.** `Quotation → SalesOrder → SalesInvoice`. Only the invoice
  recognises revenue. ([Philosophy](01-philosophy.md).)
- **Selling emits, never writes, the ledger.** On post it builds a balanced `AccountingPostEnvelope`
  and hands it to *your* `GlPostSink`. ([Architecture](04-architecture.md), [ADR-002](../adr/ADR-002-gl-posting-seam.md).)
- **Money is server-computed.** Send lines (`quantity`, `unitPrice`, `lineDiscount`); never send
  `subtotal`/`total`. The guarded surface refuses inconsistent documents.
- **Selling holds no masters.** `customerId`, `itemId`, and the account ids are **logical FKs** to
  other modules — you supply valid ids; selling does not own or join them.
- **Two lifecycles.** `status` is the document (draft/submitted/paid); `posting_state` is the GL
  reconciliation (pending/posted/failed). They move independently.

## Recipes

### How do I implement the `GlPostSink`?

In the composing service, map the envelope onto accounting's `PostingService` — this is the
anti-corruption layer:

```rust
use backbone_selling::{GlPostSink, AccountingPostEnvelope, GlPostAck, GlPostRejected};

struct AccountingSink { posting: accounting::PostingService }

#[async_trait::async_trait]
impl GlPostSink for AccountingSink {
    async fn post(&self, env: &AccountingPostEnvelope) -> Result<GlPostAck, GlPostRejected> {
        // translate env.lines[] → accounting::PostingRequest, call self.posting.post(...),
        // return GlPostAck { journal_id, post_id } on success or GlPostRejected { code, .. }.
    }
}
```

Selling never names an accounting type; only your sink does. See
[`tests/gl_posting_seam.rs`](../../tests/gl_posting_seam.rs) for the reference adapter.

### How do I run the full order-to-cash conversion?

Drive `SellingWriteService`: `accept_quotation` → `convert_quotation_to_order` (fails
`quotation_not_accepted` if the quotation isn't accepted) → `confirm_sales_order` (order → `to_bill`)
→ `create_invoice_from_order` → `post_sales_invoice` (advances each SO line's `billed_qty`; order →
`completed` when fully billed). This is [`OTC-1`](../business-flows/golden-cases.md).

### How do I react to a confirmed order (e.g. credit-limit watch)?

Subscribe to selling's domain events — don't modify selling. Implement `SellingEventSink` (or use the
`application/subscriptions` registry) and handle `SalesOrderConfirmed`; the reference
`CreditWatchConsumer` in [`consumer_credit_rule_custom.rs`](../../src/application/service/consumer_credit_rule_custom.rs)
records a breach over a limit. Selling emits; your consumer decides
([extension-contract §5](../../tests/extension_contract.rs)).

### How do I reference a customer / item / account?

By id only — they are **logical FKs**. Selling does not validate them against another module's tables
(no cross-schema constraint). Supply ids your `party`, `catalog`, and `accounting` modules consider
valid; selling stores and posts them.

### How do I seed sample data?

```bash
metaphor migration seed selling            # run Rust seeders (src/seeders/)
metaphor migration generate-seeds selling  # emit SQL seed files
```

## Configuration

Defaults live in [`config/application.yml`](../../config/application.yml); override per environment and
at runtime.

| Option | Default | When to change |
|--------|---------|----------------|
| `server.port` | `8080` | Port conflicts / multi-service hosts. |
| `database.url` | `postgresql://root:password@localhost:5432/skeletondb` | **Always** in real deployments — override with `DATABASE_URL` (takes precedence). |
| `database.max_connections` | `10` | Tune to your Postgres pool budget. |
| `entities.<name>.pagination.default_limit` | `20` (`max_limit` `100`) | Per-entity list-page sizing. |
| `logging.level` | `info` | `debug`/`trace` when diagnosing. |

`DATABASE_URL` in the environment always wins over the YAML.

## Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| `422 empty_document` | Posting a document with `lines: []` | A quotation/order/invoice needs ≥1 line. |
| `422 tax_account_missing` | `taxRate > 0` but no `taxOutputAccountId` | Supply the PPN-output account, or set `taxRate` to `0`. |
| `422 missing_revenue_account` | An invoice line has no `revenueAccountId` | Every invoice line needs an income account. |
| `422 unsupported_currency` | Invoice currency ≠ `IDR` | Revenue posts are IDR-only today; the DB `CHECK` also blocks it. |
| `422 quotation_not_accepted` | Converting a quotation not in `accepted` | Accept it first (`accept_quotation`). |
| `422 not_draft` | Re-confirming/mutating a non-draft document | The action is only valid from `draft`. |
| `405/404` on `POST /sales-invoices/bulk` or `DELETE /sales-invoices/{id}` | Generic mutation isn't mounted on the guarded surface | By design — use the validated create, or the unguarded `all_crud_routes()` in a trusted context. |
| GL post returns a `failed` invoice, no journal | Accounting rejected it (e.g. `non_postable_account`) | Fix the account reference; selling surfaces accounting's stable code verbatim. |
| `backbone-schema: command not found` | Following the stale skeleton README | Use `metaphor schema schema …`. |
| JSON field names look wrong (`created_at` vs `createdAt`) | Expecting snake_case on the wire | DTOs are `camelCase` by design; snake_case is DB/Rust only. |

---

Next: [Contributing](07-contributing.md) to send a change back, or the [Glossary](08-glossary.md) to
pin down a term.
