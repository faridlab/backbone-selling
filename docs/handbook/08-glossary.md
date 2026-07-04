<!-- Reader: All · Mode: Reference -->
# Glossary — ubiquitous language

One term, one meaning, used everywhere in this handbook and in the code. When a term here names a
type or file, that name is exact. If a doc uses a different word for one of these, the doc is the bug.
Terms are grouped: **selling domain** first, then the **framework mechanism** that underlies it.

## Selling domain

### `AccountingPostEnvelope`
The `Serialize`/`Deserialize` struct selling builds to post revenue
([`selling_gl.rs`](../../src/application/service/selling_gl.rs)). It mirrors accounting's inbound
contract (`idempotency_key`, company/branch, `source_*`, `posting_date`, currency, `posting_type`,
balanced `lines[]`) **without being an accounting type** — it *is* the versioned cross-module
contract. `build_revenue_post` refuses to produce an unbalanced or non-IDR envelope.

### Billed watermark (`billed_qty`)
The cumulative quantity invoiced against a `SalesOrderItem`. Advanced by `advance_billing_watermarks`
when a sales invoice posts. The "billing band" of the order-status model; combined with the delivered
watermark, `recompute_order_status` moves the order to `completed` only when every line is fully
billed **and** fully delivered.

### Delivered watermark (`delivered_qty`)
The cumulative quantity delivered against a `SalesOrderItem`. Advanced by `mark_delivered` — the
inbound half of the selling↔inventory delivery seam, driven by inventory's `StockDelivered`
([ADR-004](../adr/ADR-004-delivery-seam.md)). The "delivery band" of the order-status model.

### `build_revenue_post`
The deterministic builder that turns a submitted invoice into a balanced envelope:
`Dr A/R = total` (carrying the customer party) · `Cr Revenue` grouped/summed per income account
(`BTreeMap` order) · `Cr PPN Output = tax_amount` (only when tax > 0). Refuses unbalanced and non-IDR.

### GL-posting seam
The mechanism by which selling posts to the ledger without depending on it: `build_revenue_post` →
`AccountingPostEnvelope` → `GlPostSink.post(&envelope)` → the composing service's ACL adapter →
accounting's `PostingService`. Zero horizontal Cargo edges. See [ADR-002](../adr/ADR-002-gl-posting-seam.md).

### `GlPostSink`
The trait (`async fn post(&AccountingPostEnvelope) -> Result<GlPostAck, GlPostRejected>`) that
crosses the boundary to accounting. Selling calls it; the **composing service** implements it over
accounting's `PostingService`. Selling never names an accounting type.

### `GlPostingState` (`posting_state`)
The GL **reconciliation** state of a sales invoice: `pending` → `posted` / `failed`. Distinct from the
document `status`; set from the posting ack/rejection. `posted` means a journal exists in the ledger.

### Guarded routes
[`create_guarded_selling_routes(&SellingModule, pool)`](../../src/presentation/http/guarded_routes.rs)
— the **recommended** mount: read all documents + **validated creates** (`/quotations`,
`/sales-orders`, `/sales-orders/confirm`, `/sales-invoices`). Generic create/update/delete are *not*
mounted, so a caller cannot write an inconsistent or under-specified document. Contrast
`SellingModule::all_crud_routes()` (unguarded) and the `#[deprecated]` `routes()`.

### Idempotency key
`idempotency_key = invoice_id`, and the envelope's `source_id = invoice_id`. Accounting's partial
unique index `(company, source_type, source_id, posting_type) WHERE posted` is the authoritative
arbiter — two concurrent posts of one invoice yield **one** journal. A re-post never double-recognises
revenue.

### Logical foreign key
A cross-module reference declared with `@exclude_from_foreign_key_check` (masters:
`customer_id → party.Party`, `item_id → catalog.Item`, account ids → `accounting.Account`,
`company_id → organization.Company`) or `@foreign_key(module.Type.id)` for audit actors
(`→ sapiens.User.id`). It documents the relationship and is **not** a DB constraint into another
schema, so selling stays independently deployable and holds no masters.

### Order-to-cash
Selling's pipeline: `Quotation → SalesOrder → SalesInvoice`. Selling owns the *intent* (quotation,
order) and *revenue recognition* (invoice). Delivery/COGS belongs to `backbone-inventory`; payment
belongs downstream.

### PPN
Indonesian VAT (Pajak Pertambahan Nilai). Selling carries a single supplied `tax_rate` and computes
one PPN amount on the subtotal (the `Cr PPN Output` credit). Real multi-rate, effective-dated tax is
deferred to `backbone-tax` ([ADR-001](../adr/ADR-001-selling-boundary.md) §3).

### Quotation
A priced, time-boxed offer to a customer (`schema/models/quotation.model.yaml`). Head of the
pipeline; posts nothing. `QuotationStatus`: draft → sent → accepted → ordered / rejected / expired /
cancelled. An `accepted` quotation converts to a SalesOrder.

### SalesInvoice
The one document that **recognises revenue** (`schema/models/sales_invoice.model.yaml`). On post,
emits the revenue envelope. `SalesInvoiceStatus`: draft → submitted → partially_paid / paid /
cancelled. Carries `receivable_account_id`, `tax_output_account_id`, and per-line
`revenue_account_id` (all logical FKs to `accounting.Account`), plus the reconciliation fields
(`posting_state`, `journal_id`, `accounting_post_id`, `posted_at`, `outstanding_amount`).

### SalesOrder
Confirmed customer demand (`schema/models/sales_order.model.yaml`); owns intent, no GL post.
`SalesOrderStatus` is 7-state ([ADR-003](../adr/ADR-003-order-status-model.md)): `draft`, `to_bill`,
`to_deliver`, `to_deliver_and_bill`, `completed`, `closed`, `cancelled` — **all live** since the
delivery seam landed ([ADR-004](../adr/ADR-004-delivery-seam.md)). `confirm_sales_order` →
`to_deliver_and_bill`; the order then recomputes from its two watermarks toward `completed`.

### `SalesTeam` / `SalesPersonAllocation`
Commission attribution. A `SalesTeam` is a named grouping; a `SalesPersonAllocation` splits credit for
a sales order across sales people by percentage (Σ per order must be ≤ 100). `sales_person_id` is a
logical FK to `sapiens.User`.

### `SellingEvent` / `SellingEventSink`
Selling's outbound domain-event surface ([`selling_events.rs`](../../src/application/service/selling_events.rs)):
`QuotationAccepted`, `SalesOrderConfirmed`, `SalesInvoiceIssued`, `SalesInvoicePosted`. Consumers
subscribe via `SellingEventSink` and decide their own reactions (e.g. credit watch). Selling emits;
the consumer decides. The consumer's rule survives regen (extension-contract §5).

### `SellingWriteService`
The validated write path ([`selling_write_service.rs`](../../src/application/service/selling_write_service.rs)):
`create_quotation`, `create_sales_order`, `confirm_sales_order`, `accept_quotation`,
`convert_quotation_to_order`, `create_sales_invoice`, `create_invoice_from_order`,
`build_revenue_post`, `post_sales_invoice`. Computes all money server-side; returns typed
`SellingError` (`empty_document`, `tax_account_missing`, `unsupported_currency`, `not_draft`,
`quotation_not_accepted`, …) with a stable `code()` + `http_status()`. Stateless over the pool.

## Framework mechanism

### Application layer
The use-case layer (`src/application/`): services, DTOs, and selling's write path / GL seam / events.
Depends on the domain; knows nothing about HTTP or SQL.

### Audit metadata
The `metadata` JSONB field (`created_at`, `updated_at`, `deleted_at`, `created_by`, `updated_by`,
`deleted_by`) added by `config.audit: true`. Timestamps are trigger-set; `*_by` are logical FKs to
`sapiens.User.id`.

### `BackboneCrudHandler`
The `backbone-core` type that produces an Axum `Router` with all **twelve** CRUD endpoints for an
entity. Backs the generated per-entity handlers. On the guarded surface, only the *read* subset is
mounted.

### Bounded context
The single business domain a module owns. Selling = order-to-cash intent + revenue recognition. One
module = one bounded context; it never edits another's schema.

### Composition root
[`src/lib.rs`](../../src/lib.rs) — `SellingModule` + `SellingModuleBuilder`. Wires each of the eight
services to its repository and composes routers. The one place allowed to depend on every layer.

### CUSTOM marker
A `// <<< CUSTOM … // END CUSTOM` region inside a generated file whose content survives regeneration.
Spelling varies per file — match what is already there.

### DTO (Data Transfer Object)
A wire-shape struct in `src/*/dto/`. Serialized `camelCase`. Generated, with `From`/`Apply`
conversions. (The guarded write path uses its own hand-written request bodies in `guarded_routes.rs`.)

### Domain layer
The innermost layer (`src/domain/`): entities, value objects, the status enums, and repository
**traits** (ports). Depends on nothing.

### `GenericCrudRepository` / `GenericCrudService`
The `backbone-orm` / `backbone-core` generics carrying all standard CRUD. Each selling repository is a
**newtype** over `GenericCrudRepository`; each service is a **type alias** over `GenericCrudService`.
Inherited, never re-implemented.

### Infrastructure layer
The adapter layer (`src/infrastructure/`): the eight repository implementations and the event store.
Depends on domain and application.

### `metaphor`
The workspace CLI (v0.2.0) that orchestrates projects and dispatches to plugins (`metaphor-schema`,
`metaphor-codegen`, `metaphor-dev`). Prefer it over raw `cargo`/`sqlx`. The standalone
`backbone-schema` binary the skeleton README mentions is **not** installed.

### Module
A **library crate** owning one bounded context in 4-layer DDD, schema-driven. `[lib]` only — no
`main.rs`. Composed into a `backend-service`; never run alone. `backbone-selling` is one.

### Own schema (per module)
Each module gets its own Postgres schema (`schema: selling` in `index.model.yaml`). Migrations
`CREATE SCHEMA selling` and qualify tables as `selling.<table>`, so selling and accounting never
collide on a table name.

### Port / Adapter
The DDD names for the two repositories per entity: the **port** is the domain-layer `trait`; the
**adapter** is the infrastructure-layer newtype over `GenericCrudRepository`.

### Presentation layer
The transport layer (`src/presentation/`, `src/routes/`): generated CRUD handlers, read-route helpers,
and the hand-written `create_guarded_selling_routes`.

### Regeneration (regen)
Re-running `metaphor schema schema generate … --force` to rebuild downstream code from the schema.
Overwrites everything **outside** a protected region (CUSTOM markers, `user_owned` globs). Selling's
regen-safety is proven by `scripts/regen_roundtrip.sh`.

### Schema (the SSoT)
`schema/models/*.model.yaml` — the single source of truth. Not to be confused with the *Postgres
schema* (the per-module namespace).

### Soft delete
Marking a row deleted (`metadata.deleted_at` set) instead of removing it (`config.soft_delete: true`).
Unique document-number indexes are `WHERE deleted_at IS NULL`, so a number frees up after a soft
delete.

### Twelve endpoints
The standard CRUD surface an entity gets from `BackboneCrudHandler`: `list`, `create`, `get`,
`update`, `patch`, `soft_delete`, `restore`, `empty_trash`, `bulk_create`, `upsert`, `find_by_id`,
`list_deleted`. On the guarded surface only the read subset is mounted; the writes are replaced by the
validated write path.

### `user_owned`
The `metaphor.codegen.yaml` key listing glob paths the generator skips wholesale — never reads,
merges, or deletes. Selling's write path, GL seam, events, guarded routes, tests, the IDR-only
migration, and `docs/**` are all `user_owned`.
