# backbone-selling

> **Selling: Quotation → Sales Order → Sales Invoice.** Owns the order-to-cash *intent*; the sales
> invoice recognises revenue by **emitting** a balanced posting (`Dr A/R · Cr Revenue · Cr PPN
> Output`) into `backbone-accounting` through the GL-posting seam — without importing the ledger and
> without holding a single master record.

A Backbone Framework **domain module**: a library crate (`[lib]` only, no `main.rs`) that a
`backend-service` composes. Schema YAML is the single source of truth; most of the code is generated
from it, and the hand-written logic (the validated write path, the GL seam, the domain events) lives
in regen-safe files.

**📖 Start with the [handbook](docs/README.md)** — philosophy, architecture, the developer guide, and
the ADRs. This README is the 60-second orientation.

## The domain in one picture

```
Quotation ──accept──▶ SalesOrder ──create invoice──▶ SalesInvoice ──post──▶ backbone-accounting
 (offer)              (confirmed demand)              (recognises revenue)      (the General Ledger)
  no GL post            no GL post                     emits AccountingPostEnvelope via GlPostSink
```

- **Eight entities:** `Quotation`(+`Item`), `SalesOrder`(+`Item`), `SalesInvoice`(+`Item`),
  `SalesTeam`(+`SalesPersonAllocation`).
- **Holds no masters.** `customer` → `party.Party`, `item` → `catalog.Item`, accounts →
  `accounting.Account`, `company` → `organization` — all **logical FKs**, no DB constraint, zero
  horizontal Cargo edges.
- **GL producer, never the ledger.** Only the invoice posts, and it posts a *serialized envelope*
  through the [`GlPostSink`](src/application/service/selling_gl.rs) trait — the composing service maps
  it onto accounting's `PostingService`. Idempotent (`source_id = invoice_id`), always balanced,
  **IDR-only**.
- **Money is computed server-side.** The guarded write surface refuses inconsistent documents.

## Layout (what's generated vs. hand-written)

```
schema/models/*.model.yaml     ← SOURCE OF TRUTH (quotation, sales_order, sales_invoice, sales_team)
migrations/                    ← generated, + one hand-written IDR-only CHECK (…020)
src/
├── lib.rs                     ← SellingModule + SellingModuleBuilder (the composition root)
├── domain/                    ← entities, status enums, repository traits            (generated)
├── application/
│   └── service/
│       ├── *_service.rs                    ← 8 type aliases over GenericCrudService   (generated)
│       ├── selling_write_service.rs  ★     ← validated writes, totals, order-to-cash, posting
│       ├── selling_gl.rs             ★     ← AccountingPostEnvelope, GlPostSink, build_revenue_post
│       └── selling_events.rs         ★     ← SellingEvent + SellingEventSink (extension surface)
├── infrastructure/persistence/  ← 8 repository newtypes over GenericCrudRepository    (generated)
└── presentation/http/
    ├── *_handler.rs                        ← generated CRUD handlers                  (generated)
    └── guarded_routes.rs           ★       ← create_guarded_selling_routes (RECOMMENDED mount)
tests/                          ★ selling_golden_cases · gl_posting_seam · integrity_probes ·
                                  order_to_cash · extension_contract   (the golden oracle)
docs/                           ← the handbook + ADRs + business flows
```

`★` = hand-written, listed under `user_owned` in [`metaphor.codegen.yaml`](metaphor.codegen.yaml);
`metaphor schema generate --force` never touches them. Everything else regenerates from the schema.

## Quick start

```bash
export DATABASE_URL="postgresql://root:password@localhost:5432/skeletondb"

metaphor schema schema validate      # schema is well-formed
metaphor migration run               # create the selling.* schema, 8 tables, triggers, IDR check
metaphor dev test                    # unit + golden + seam + probe oracle
```

Integrate it into a service:

```rust
let selling = backbone_selling::SellingModule::builder()
    .with_database(pool.clone())
    .build()?;

// RECOMMENDED: read all documents + validated creates. Generic mutation is NOT mounted.
let routes = backbone_selling::create_guarded_selling_routes(&selling, pool.clone());
let app = axum::Router::new().nest("/api/v1", routes);
```

The [Developer Guide](docs/handbook/06-developer-guide.md) walks a quotation → posted-invoice run and
shows how to implement the `GlPostSink`.

## Custom code (regeneration safety)

Two mechanisms keep hand-written code alive across `metaphor schema generate --force`:

1. **`// <<< CUSTOM … // END CUSTOM` markers** inside a generated file (e.g. the builder in
   `lib.rs`) — content between the markers is preserved.
2. **`user_owned` globs** in [`metaphor.codegen.yaml`](metaphor.codegen.yaml) — whole files the
   generator skips (all of selling's real logic lives here).

`scripts/regen_roundtrip.sh` proves the `user_owned` files are byte-identical after a regen. See the
[Maintainer Guide](docs/handbook/05-maintainer-guide.md).

## Dependencies

The `backbone-*` framework crates are **git dependencies** on `branch = "main"` (no path fix-up
needed). Pin them to a tag/rev for a reproducible release build. See
[Cargo.toml](Cargo.toml) and the [Technology page](docs/handbook/03-technology.md).

## Key decisions

- [ADR-001](docs/adr/ADR-001-selling-boundary.md) — three documents; GL producer, holds no masters.
- [ADR-002](docs/adr/ADR-002-gl-posting-seam.md) — the envelope + `GlPostSink` ACL; idempotent, IDR-only.
- [ADR-003](docs/adr/ADR-003-order-status-model.md) — the 7-state order model; billing live, delivery dark.
