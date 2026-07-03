<!-- Reader: Contributor · Mode: How-to -->
# Contributing

How to land a change in `backbone-selling` — dev setup, conventions, and the checklist a reviewer
will hold you to. The single hardest rule to remember: **commit messages carry no Claude or
co-author signature.** Everything else is standard.

## Dev setup

```bash
# 1. Toolchain
rustup show                 # Rust 2021 edition toolchain
metaphor --version          # metaphor 0.2.0+ on PATH

# 2. A database for tests (the seam test also needs the accounting.* schema present)
export DATABASE_URL="postgresql://root:password@localhost:5432/skeletondb"
metaphor migration run

# 3. Prove a clean baseline before you change anything
metaphor dev test
metaphor lint check
bash scripts/regen_roundtrip.sh   # user_owned files survive a regen
```

If `metaphor` is not installed, see the workspace `metaphor.yaml` / plugin discovery order
(`$PATH` → `$METAPHOR_PLUGIN_BIN_DIR` → `~/.metaphor/bin/`).

## The golden rule of module changes

You are almost never editing generated Rust directly. Before writing code, ask: *does this belong in
the schema?* If it changes an entity's shape, the answer is yes — edit
`schema/models/*.model.yaml`, regenerate, and commit the regenerated output *with* the schema change.
A PR that hand-edits a generated struct will be sent back. Selling's real logic lives in the
`user_owned` files (the write path, the GL seam, the events, the guarded routes) — that is where your
hand-written change usually goes. See the [Maintainer Guide](05-maintainer-guide.md).

## Branch & commit conventions

- **Branch** off `main`. Never commit directly to `main`.
- **Conventional commits.** `type(scope): summary` — e.g. `feat(invoice): reverse post on cancel`,
  `fix(order): correct billed_qty watermark`, `docs(handbook): specialize architecture to selling`.
  Types drive versioning: `fix:` → patch, `feat:` → minor, `feat!:`/`BREAKING CHANGE:` → major.
  **A change to the `AccountingPostEnvelope` shape is `feat!:`** — it is the cross-module contract.
- **One concern per commit.** Group by functionality; keep large regenerated files in their own
  commit rather than mixed with hand-written logic.
- **Message says *why*, not "update".** No filler (`wip`, `fix stuff`, `changes`).
- **NO signatures.** Never append `Co-Authored-By`, `Generated with…`, or any trailer. Hard
  workspace rule (root `CLAUDE.md`).

```
feat(invoice): reject a non-IDR revenue post

build_revenue_post now returns unsupported_currency (422) for currency != IDR,
backed by a CHECK on selling.sales_invoices. A foreign invoice can no longer
book face value into the IDR ledger (ADR-002 §6).
```

## Before you open a PR — the checklist

- [ ] Change started in the **schema YAML** if it touches an entity's shape.
- [ ] `metaphor schema schema validate selling` passes.
- [ ] Regenerated code committed alongside the schema change (no hand-edits outside protected regions).
- [ ] Custom logic lives in a `// <<< CUSTOM` marker or a `user_owned` file; new `user_owned` paths
      added to [`metaphor.codegen.yaml`](../../metaphor.codegen.yaml) in the same commit.
- [ ] `scripts/regen_roundtrip.sh` green — `user_owned` files byte-identical after a regen.
- [ ] **No `use` of `backbone_accounting`** — `cargo tree -e normal -i backbone-accounting` empty.
- [ ] Any new GL post is **balanced** and **IDR-only** (`build_revenue_post` invariants upheld).
- [ ] Money/GL changes updated the numeric oracle in
      [`docs/business-flows/golden-cases.md`](../business-flows/golden-cases.md) and its tests.
- [ ] No `main.rs` / binary target added (this is a **library**).
- [ ] No hand-rolled Axum CRUD — generated handlers / the guarded write path used.
- [ ] No sibling module's schema touched; cross-module references are logical FKs.
- [ ] `metaphor dev test` green; `metaphor lint check` clean.
- [ ] New/changed behavior has a test; a bug fix has a test that fails without the fix.
- [ ] Migrations have both `*.up.sql` and `*.down.sql`.
- [ ] Conventional-commit messages, **no signatures**.

## Tests — the golden oracle

Selling's tests are the authored-first oracle, not an afterthought. They live in `user_owned` files:

- `tests/selling_golden_cases.rs` — the write-path numeric oracle (`SGC-*`).
- `tests/integrity_probes.rs` — the guarded-surface probes (`IGC-*`: generic mutation is *not*
  mounted).
- `tests/gl_posting_seam.rs` — selling → the **real** accounting ledger (`SEAM-*`), including the
  concurrent-double-post-yields-one-journal proof.
- `tests/order_to_cash.rs` — the quotation→order→invoice conversion (`OTC-*`).
- `tests/extension_contract.rs` + `scripts/regen_roundtrip.sh` — the consumer + regen round-trip
  (`EXT-*`).
- `tests/features/**` — BDD features (`user_owned`).

Keep the three in step: a business flow in [`docs/business-flows/`](../business-flows/README.md) ↔ a
`.feature` ↔ a golden-case test. If you change the numbers, change all three.

## Review expectations

A reviewer checks, in order:

1. **Did the change start in the right place?** Schema for shape; a `user_owned` service file for
   logic.
2. **Regen-safety.** Nothing valuable sits where the next `generate --force` would eat it;
   `regen_roundtrip.sh` still green.
3. **Boundary integrity.** No accounting import; every GL post balanced and IDR-only; masters
   referenced by logical FK.
4. **Layer discipline.** Domain imports nothing transport/DB; arrows point inward.
5. **Consistency.** Terms match the [Glossary](08-glossary.md); the guarded surface is used, not
   hand-rolled CRUD.
6. **Proof.** Golden cases updated and green; migrations reversible.

Expect a request to move logic into a protected region if it's in generated territory — the most
common round-trip, and not a nit.

## Architectural changes

If your change is a *decision* (a new dependency, a new GL post shape, a status-model change), write
an ADR. Selling keeps two ADR sets: framework decisions in [`adr/`](adr/) and selling-domain
decisions in [`../adr/`](../adr/) (boundary, GL seam, order-status). ADRs are immutable once accepted;
supersede rather than edit.

---

Related: [Glossary](08-glossary.md) · [Maintainer Guide](05-maintainer-guide.md) · [ADRs](adr/).
