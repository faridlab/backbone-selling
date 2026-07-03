<!-- Reader: Maintainer · Mode: Explanation -->
# Provenance & reuse

This handbook has been **specialized to the concrete `backbone-selling` module**. It documents the
real order-to-cash domain — Quotation → Sales Order → Sales Invoice, the GL-posting seam, the guarded
write surface — not the generic skeleton the module was stamped from. Every page names one reader and
one Diátaxis mode; every command was run against `metaphor 0.2.0`; where the schema/code and a doc
disagree, the code wins and the doc is a bug.

## Where selling came from

Selling was scaffolded from the **Backbone module skeleton** (one `Example` entity wired end-to-end)
and then built out into a domain module. Because `docs/**` is a `user_owned` path in
[`metaphor.codegen.yaml`](../../metaphor.codegen.yaml), this handbook travels with the module and is
never rewritten by the generator — it is maintained by hand alongside the schema and code.

The skeleton's reference-entity artifacts — the `Example` composition root (`src/module.rs`), the
`src/**/example_*.rs` files, and `tests/features/example.feature` — have been **removed**. The
composition root is `SellingModule` in [`src/lib.rs`](../../src/lib.rs), and every entity in `src/`
is a real selling concept.

## Reusing selling as a template for the next producer

Selling is the **reference implementation of the GL-posting seam** ([ADR-002](../adr/ADR-002-gl-posting-seam.md)).
If you are building the next transactional producer (Buying → Inventory COGS, a split-out Billing),
these parts are meant to be copied and re-pointed, not reinvented:

| Reusable part | File | What to re-point |
|---------------|------|------------------|
| The envelope + sink contract | [`selling_gl.rs`](../../src/application/service/selling_gl.rs) | Your post shape (`build_*_post`) and its `posting_type`; keep the balance + currency guards |
| The validated write path pattern | [`selling_write_service.rs`](../../src/application/service/selling_write_service.rs) | Your documents and totals; keep server-side money + typed errors |
| The guarded surface pattern | [`guarded_routes.rs`](../../src/presentation/http/guarded_routes.rs) | Your validated creates; keep "generic mutation not mounted" |
| The event surface | [`selling_events.rs`](../../src/application/service/selling_events.rs) | Your domain events; keep "emit; consumers decide" |
| The regen round-trip proof | `scripts/regen_roundtrip.sh` + `user_owned` list | Your file set |

What stays **framework-generic** across any module: the schema-YAML SSoT, generic CRUD, the CUSTOM /
`user_owned` regen-safety mechanism, the 4-layer DDD shape, and the contribution conventions. Those
are documented in [Philosophy](01-philosophy.md) §1, the three seed [ADRs](adr/), and the
[Maintainer Guide](05-maintainer-guide.md).

---

Back to the [handbook index](../README.md).
