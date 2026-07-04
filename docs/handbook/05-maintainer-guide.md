<!-- Reader: Maintainer · Mode: How-to -->
# Maintainer Guide

How to maintain `backbone-selling` and add features without breaking the regeneration machine — or
the GL boundary. If you read one rule, read this: **edit the schema YAML, then regenerate; put
hand-written code only where the generator promises not to touch it; and never let selling import
accounting.**

All commands below were run against `metaphor 0.2.0`. Where the skeleton README differs, this guide
has the working form.

## Before you touch anything

- Read this project's [`CLAUDE.md`](../../CLAUDE.md) and the workspace `metaphor.yaml`.
- Confirm the project type is **`module`** — a **library** (`[lib]` only). Never add a `main.rs`.
- Internalize the source of truth: **`schema/models/<entity>.model.yaml`**. Code is downstream.
- Know the two boundaries you must not cross: (1) selling references other modules by **logical FK**,
  never a DB constraint or a Cargo dependency; (2) selling **emits** to the ledger via `GlPostSink`,
  never writes a journal row.

## Where code goes (and what it may depend on)

| Layer | Directory | Put here | May depend on |
|-------|-----------|----------|---------------|
| Domain | `src/domain/` | Entities, value objects, invariants, repository **traits** | nothing |
| Application | `src/application/` | Services (type aliases), DTOs, the write path, the GL seam, events | domain |
| Infrastructure | `src/infrastructure/` | Repository impls, event store | domain, application |
| Presentation | `src/presentation/`, `src/routes/` | HTTP handlers, guarded route composition | application |

Dependency arrows point inward. If the domain layer imports `axum` or `sqlx`, something is in the
wrong layer.

## The map of what's generated vs. hand-written

Selling's hand-written value is a small, `user_owned` set (from [`metaphor.codegen.yaml`](../../metaphor.codegen.yaml)).
Everything else is generated. Know this map before you edit:

| Hand-written (`user_owned` — regen never touches) | What it is |
|--------------------------------------------------|------------|
| `src/application/service/selling_write_service.rs` | The validated write path: totals, conversion, `post_sales_invoice` |
| `src/application/service/selling_gl.rs` | `AccountingPostEnvelope`, `GlPostSink`, `build_revenue_post` |
| `src/application/service/selling_events.rs` | `SellingEvent`, `SellingEventSink` (the extension surface) |
| `src/application/service/consumer_credit_rule_custom.rs` | The reference consumer (extension-contract §5) |
| `src/presentation/http/guarded_routes.rs` | `create_guarded_selling_routes` |
| `tests/{selling_golden_cases,gl_posting_seam,integrity_probes,extension_contract,order_to_cash,delivery_seam}.rs`, `tests/features/**` | The behavior oracle |
| `migrations/…020_sales_invoice_idr_only_check.*` | The IDR-only CHECK constraint |
| `docs/**` | This handbook |

Everything under `src/domain`, `src/infrastructure`, the generated `src/presentation/http/*_handler.rs`,
the `…_service.rs` type aliases, and `migrations/…0001`–`…0011` is **generated** — edit the schema,
not the file.

## Adding a new entity (the golden path)

Say selling needs a `CreditNote`.

```bash
# 1. Describe it — scaffold a schema stub…
metaphor make entity CreditNote --module selling
#    …or copy an existing model (e.g. schema/models/sales_invoice.model.yaml) → credit_note.model.yaml,
#    edit it, then add `- credit_note.model.yaml` under `imports:` in schema/models/index.model.yaml.

# 2. Validate before generating.
metaphor schema schema validate selling

# 3. Generate all artifacts (entity, DTOs, repo, service, handler, routes).
metaphor schema schema generate selling --target all --force

# 4. Generate + apply the migration.
metaphor migration generate CreditNote selling
metaphor migration run

# 5. Register the service in the composition root (below), then:
metaphor dev test
```

> `selling` is auto-detected from the current directory when omitted. `--target` accepts a
> comma-separated subset (e.g. `--target dto,handler`); `--dry-run` shows the diff without writing.

### Step 5 in detail — wire the service into `SellingModule`

Generation does **not** edit the composition root. Open [`src/lib.rs`](../../src/lib.rs) (the
`SellingModule` root) and follow the existing eight-service pattern:

```rust
pub struct SellingModule {
    pub quotation_service: Arc<QuotationService>,
    // …the other seven…
    pub credit_note_service: Arc<CreditNoteService>,   // ← add the field
}

// in SellingModuleBuilder::build():
let credit_note_repository = Arc::new(CreditNoteRepository::new(db_pool.clone()));
let credit_note_service = Arc::new(CreditNoteService::with_repository(credit_note_repository.clone()));
// …return it in the SellingModule { … } literal, inside the // <<< CUSTOM markers if adding by hand…

// re-export at the top of lib.rs:
pub use application::service::CreditNoteService;
```

The builder already has `// <<< CUSTOM … // END CUSTOM` markers in the struct literal and the build
body — put a hand-added field there so it survives regen. Then mount it: add it to
`all_crud_routes()` (unguarded) and/or add a read/validated-write route to
[`guarded_routes.rs`](../../src/presentation/http/guarded_routes.rs).

## Changing an existing entity

1. Edit the field in `schema/models/<entity>.model.yaml` (never the generated struct).
2. `metaphor schema schema validate selling`.
3. `metaphor migration generate <Entity> selling` (or a schema-diff migration against a live DB:
   `metaphor schema schema migration selling --database-url …`).
4. `metaphor schema schema generate selling --target all --force`.
5. `metaphor migration run && metaphor dev test`.

If your change touches money or the GL post, **update the golden cases too** — the numeric oracle in
[`docs/business-flows/golden-cases.md`](../business-flows/golden-cases.md) and the tests it mirrors
are the contract. A totals change with no golden-case update will (correctly) be sent back in review.

## Regen-safety — the three protected mechanisms

Regeneration **overwrites everything outside a protected region.** Know which one you're using.

### 1. `// <<< CUSTOM … // END CUSTOM` markers (inside generated files)

The generator preserves whatever sits between the markers. `lib.rs`'s builder ships empty ones ready
to fill; `mod.rs` files use them to keep a `pub mod`/`pub use` for a hand-written sibling. Marker
spelling varies by file (`// <<< CUSTOM`, `// <<< CUSTOM METHODS START >>>`, `// <<< CUSTOM HANDLERS
START >>>`) — **match what's already in the file**; don't invent new marker text. Use markers for
small additions: a re-export, one helper, a builder field.

### 2. `user_owned` whole files (never generated)

For anything substantial — the write path, the GL seam, the guarded routes — write a whole file the
generator never emits and list it under `user_owned` in [`metaphor.codegen.yaml`](../../metaphor.codegen.yaml).
This is how *all* of selling's real logic is protected. When you add a new hand-authored service or
route file, add its path here in the same commit:

```yaml
user_owned:
  - "src/application/service/selling_write_service.rs"
  - "src/presentation/http/guarded_routes.rs"
  # …add your new file here…
```

**Which to reach for:** a few lines → a CUSTOM marker; a cohesive unit → a `user_owned` file.

The regen round-trip is *tested*: `scripts/regen_roundtrip.sh` runs `generate --force` and asserts all
ten `user_owned` files are byte-identical afterward (extension-contract §5). Run it after any generator
or schema change.

## Adding custom selling logic (the common case)

Selling's real work is validated writes and the GL seam — both already `user_owned`. To extend:

- **A new validated operation** (e.g. "credit an invoice") → add a method to `SellingWriteService` in
  [`selling_write_service.rs`](../../src/application/service/selling_write_service.rs) and a route in
  [`guarded_routes.rs`](../../src/presentation/http/guarded_routes.rs). Return a typed
  `SellingError` with a stable `code()` + `http_status()`.
- **A new GL post shape** (e.g. a reversal on cancellation) → add a builder in
  [`selling_gl.rs`](../../src/application/service/selling_gl.rs) that returns a **balanced**
  `AccountingPostEnvelope`; drive it through the same `GlPostSink`. Never write a journal row.
- **A new domain event** for consumers → add a variant to `SellingEvent` in
  [`selling_events.rs`](../../src/application/service/selling_events.rs) and publish it from the write
  path. Consumers subscribe without selling knowing they exist (extension-contract §5).

Never add a raw Axum CRUD route or bypass `GenericCrudRepository` for standard CRUD — extend, don't
replace.

## Build, test, lint

```bash
metaphor dev test          # unit + integration + the golden/seam/probe oracle
metaphor lint check        # clippy + fmt policy
bash scripts/regen_roundtrip.sh   # prove user_owned files survive a regen
```

Never run bare `cargo build`/`cargo test` from the **workspace root** — each project has its own
`Cargo.toml`. Inside *this* module directory, `cargo test` works, but `metaphor dev test` is preferred.

## Versioning & release

- Selling is versioned in [`Cargo.toml`](../../Cargo.toml) (`0.1.3` today). Bump per conventional
  commits: `fix:` → patch, `feat:` → minor, `feat!:`/`BREAKING CHANGE` → major. **A change to the
  `AccountingPostEnvelope` shape is a breaking contract change** — treat it as `feat!:` and note it
  in an ADR.
- Before releasing: `metaphor dev test` and `metaphor lint check` clean; `regen_roundtrip.sh` green.
- Pin the `backbone-*` git deps to a tag/rev for any release build (see [Technology](03-technology.md)).
- Commits carry **no Claude / co-author signature** — see [Contributing](07-contributing.md).

## What will break things

- **Editing generated code outside a protected region** — silently overwritten on the next
  `generate --force`. The number-one regression.
- **Importing accounting** — adds the horizontal Cargo edge the whole design forbids; the seam test
  (`cargo tree … -i backbone-accounting` is empty) will fail.
- **Emitting an unbalanced or non-IDR envelope** — `build_revenue_post` refuses both; don't work
  around it.
- **Collapsing the order-status model** — the 7 states encode delivery/billing watermarks
  ([ADR-003](../adr/ADR-003-order-status-model.md)); don't "simplify" them without an ADR.
- **Adding `main.rs` / a binary target** — wrong project type; selling is a library.
- **Touching a sibling module's schema** — reference by logical FK, never edit theirs.

---

Next: [Developer Guide](06-developer-guide.md) if you are integrating selling into a service rather
than maintaining it.
