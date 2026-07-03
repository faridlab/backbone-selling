<!-- Reader: Evaluator + Maintainer · Mode: Explanation -->
# Technology & the "why"

Every dependency in [`Cargo.toml`](../../Cargo.toml) earns its place. This page gives each
significant choice a one-line rationale and names the alternative that was rejected, so an evaluator
can judge the stack and a maintainer knows *why* not to swap a piece out casually. Two choices carry
disproportionate weight for selling specifically — **`rust_decimal`** (money must be exact) and
**`serde`** (the GL envelope *is* the cross-module contract) — so they get their own sections.

Versions below are what selling pins at **v0.1.3**; where behavior is version-specific, the version
is called out.

## The choices

| Layer | Choice | Why | Rejected alternative |
|-------|--------|-----|----------------------|
| Language | **Rust 2021**, `[lib]` only | Memory safety + a type system strong enough to make generated code *provably* consistent; no GC pauses in a service hot path | Go (weaker types for the generated-DTO story), Kotlin (the mobile edge, not the domain core) |
| Async runtime | **Tokio 1.x** (`full`) | The de-facto async runtime; Axum and SQLx both build on it, so there is one reactor | `async-std` (smaller ecosystem, no Axum/SQLx alignment) |
| HTTP | **Axum 0.7** (+ `tower`, `tower-http`) | Composes as a plain `Router` — exactly what the generated handlers and selling's `create_guarded_selling_routes` return and merge | `actix-web` (its actor model fights the compose-a-Router design) |
| Database | **PostgreSQL** via **SQLx 0.8** | Queries **checked at compile time** against the schema; native enum, `uuid`, `numeric`, `jsonb`; one connection serves both `selling.*` and `accounting.*` schemas in the seam | Diesel (heavier macro layer, less async-native), any runtime-only ORM |
| **Money** | **`rust_decimal` 1.36** (`@precision(18,2)`) | Exact base-2-safe decimals; selling computes `line_amount`/`subtotal`/`tax`/`total` at 2dp round-half-up and posts them to the ledger — see below | `f64` (rounding bugs would unbalance the GL) |
| **Contract** | **`serde` / `serde_json`** | The `AccountingPostEnvelope` is `Serialize`/`Deserialize` — the **versioned wire contract** to accounting, not a shared struct; DTOs derive `camelCase` — see below | a shared Rust type across the boundary (would create the Cargo edge selling forbids) |
| Domain errors | **`thiserror` 1.0** | Typed `SellingError` variants (`empty_document`, `tax_account_missing`, `unsupported_currency`, …) that the write path maps to HTTP status + a **stable error code** | `anyhow` for domain errors (loses the typed variants the handler and tests match on) |
| Boundary errors | **`anyhow` 1.0** | Right tool at the composition boundary (`SellingModuleBuilder::build` returns `anyhow::Result`) where a typed enum adds no value | `thiserror` everywhere (ceremony with no payoff at the boundary) |
| IDs / time | **`uuid` v4**, **`chrono`** | UUID PKs avoid enumeration and merge cleanly across modules (a `customer_id` is meaningful without a join); `chrono` for audit + document dates | integer PKs (leak ordinality, collide across modules) |
| Config | **`config` 0.14** + **`serde_yaml`** | Layered YAML (`application.yml` + env overrides); `DATABASE_URL` overrides at runtime | hardcoded config, bespoke env parsing |
| Validation | **`validator` 0.16** (feature-gated) | Declarative DTO field rules from the schema (`@max(40)` → `#[validate(length(max = 40))]`) at the edge | hand-written guard clauses scattered across handlers |
| Logging | **`tracing`** (+ `tracing-subscriber`) | Structured, async-aware spans; the composing service installs the subscriber | `log` (no span/async context) |
| gRPC / proto | **`tonic` 0.12` + `prost`** present, **generation disabled** | The deps are in `Cargo.toml`, but selling's [`index.model.yaml`](../../schema/models/index.model.yaml) sets `generators.disabled: [graphql, grpc, proto]` — selling ships **REST-only** today | generating a second transport before there's a consumer for it |

> **gRPC note.** Do not assume a Protobuf surface exists because `tonic`/`prost` are dependencies.
> `config.generators.disabled` in the schema turns off `graphql`, `grpc`, and `proto` generation for
> selling. Re-enable them there (not by hand-writing `.proto`) if a gRPC consumer ever needs them.

## Why `rust_decimal`, not `f64` — the money invariant

Selling's whole reason to exist is to put correct numbers in the ledger. Every money field is
`decimal` with `@precision(18,2)` (line quantities use `@precision(18,4)`), which generates
`rust_decimal::Decimal` and a Postgres `NUMERIC(18,2)` column. The validated write path
([`selling_write_service.rs`](../../src/application/service/selling_write_service.rs)) computes
`line_amount = money(quantity · unit_price) − line_discount` and the document totals at **2 decimals,
round-half-up**, server-side. The golden cases pin the exact arithmetic — e.g. subtotal `100.05` at
`11%` PPN yields tax `11.01` (11.0055 rounded half-up) and total `111.06`, and the envelope still
balances exactly ([`SGC-4`](../business-flows/golden-cases.md)). `f64` cannot represent `100.05`
exactly; a rounding error there would land an unbalanced journal in accounting. That is why the
choice is not negotiable here.

## Why `serde` is the contract, not a shared struct

The GL-posting seam ([ADR-002](../adr/ADR-002-gl-posting-seam.md)) forbids selling from importing any
accounting type. The mechanism that makes that possible is serialization: `AccountingPostEnvelope`
(defined in [`selling_gl.rs`](../../src/application/service/selling_gl.rs)) is a `Serialize` /
`Deserialize` struct that *mirrors* accounting's inbound contract shape (`idempotency_key`,
company/branch, `source_*`, `posting_date`, currency, `posting_type`, balanced `lines[]`) without
*being* an accounting type. The composing service's `GlPostSink` deserializes it and maps it onto
accounting's real `PostingService` — the anti-corruption layer. **The envelope is the versioned
contract.** Change its shape and you have changed the contract deliberately; you have not silently
recompiled two modules into lockstep.

## The framework crates

Four crates carry the leverage. In selling they are **git dependencies** on the public framework repo,
pinned to `branch = "main"`:

```toml
backbone-core      = { git = "https://github.com/faridlab/backbone-framework", branch = "main", features = ["postgres"] }
backbone-orm       = { git = "https://github.com/faridlab/backbone-framework", branch = "main" }
backbone-auth      = { git = "https://github.com/faridlab/backbone-framework", branch = "main" }
backbone-messaging = { git = "https://github.com/faridlab/backbone-framework", branch = "main" }
```

| Crate | Gives selling | Seen in the code as |
|-------|---------------|---------------------|
| **`backbone-core`** | `GenericCrudService`, `BackboneCrudHandler`, `PersistentEntity`, DTO conversions, `ServiceError`/`ServiceResult` | the eight service type aliases, the generated handlers, `service/error.rs` |
| **`backbone-orm`** | `GenericCrudRepository`, `SoftDelete`, pagination types | the eight repository newtypes, each entity's `EntityRepoMeta` |
| **`backbone-auth`** | identity / permission primitives | the `application/auth/*` per-entity auth stubs |
| **`backbone-messaging`** | message-bus adapters | the domain-event surface (`selling_events.rs`, `application/subscriptions/`) |

> **Reproducibility note.** `branch = "main"` is convenient but *not reproducible* — a fresh
> `cargo build` can pull a newer commit. For anything you ship, pin to a tag or commit
> (`tag = "vX.Y.Z"` or `rev = "<sha>"`). `Cargo.lock` is committed, which pins transitively, but the
> git ref is what a `cargo update` will move.

> ⚠️ **Doc drift flagged.** The top-level [README](../../README.md) is the *skeleton's* README and
> calls the `backbone-*` crates "path dependencies … [that] must point at your actual checkout." In
> selling they are **git dependencies** (see the `Cargo.toml` comment). Follow the `Cargo.toml`, not
> that README step.

## The CLI: `metaphor`, not `backbone-schema`

Generation, migration, and testing go through the **`metaphor`** binary (v0.2.0 at time of writing),
which dispatches to plugins (`metaphor-schema`, `metaphor-codegen`, `metaphor-dev`).

> ⚠️ **Doc drift flagged.** The skeleton README invokes a standalone `backbone-schema` binary and
> `backbone migration run`. Those are **stale** — `backbone-schema` is not on `PATH`. The working
> forms are `metaphor schema schema generate …` and `metaphor migration run`; the
> [Developer Guide](06-developer-guide.md) and [Maintainer Guide](05-maintainer-guide.md) use the
> verified commands throughout.

Why a workspace CLI instead of raw `cargo`/`sqlx`? Because selling never lives alone — it is one
project in a multi-project workspace, and `metaphor` applies workspace-wide policy (affected-only
builds, cross-project codegen, plugin discovery). See the schema docs'
[INTEGRATION](../schema/INTEGRATION.md) for how the pieces compose.

---

Next: [Architecture](04-architecture.md) — the C4 view, the real 4-layer shape, and an invoice
post traced through the GL seam.
