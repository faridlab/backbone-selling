# backbone-selling — Handbook

The documentation set for the **`backbone-selling`** domain module: the order-to-cash intent of an
ERP — Quotation → Sales Order → Sales Invoice — that recognises revenue by **emitting** a balanced
posting into the General Ledger through the [GL-posting seam](adr/ADR-002-gl-posting-seam.md), while
holding no masters and importing no sibling module.

Every page below names **one reader** and **one mode** (Diátaxis) at its top. Find your reader,
follow the path.

## Find your path

| You are… | You want to… | Start here |
|----------|--------------|-----------|
| **Evaluator** | Decide whether to build on / borrow this | [Philosophy](handbook/01-philosophy.md) → [Background](handbook/02-background.md) → [Technology](handbook/03-technology.md) |
| **App developer** | Integrate selling into a service and drive it | [Developer Guide](handbook/06-developer-guide.md) |
| **Maintainer** | Understand the machine and extend it safely | [Architecture](handbook/04-architecture.md) → [Maintainer Guide](handbook/05-maintainer-guide.md) |
| **Contributor** | Open a correct PR | [Contributing](handbook/07-contributing.md) |
| **Anyone** | Agree on what a word means | [Glossary](handbook/08-glossary.md) |

## The handbook

0. [Provenance & reuse](handbook/00-adapting-to-your-module.md) — *Maintainer.* Where selling came from (the skeleton), which artifacts are leftovers, and how to reuse the GL seam for the next producer.
1. [Philosophy & motivation](handbook/01-philosophy.md) — *Evaluator.* Two convictions: plumbing is generated; selling produces accounting but is never the ledger. The honest non-goals.
2. [Background & prior art](handbook/02-background.md) — *Evaluator.* The ERP-selling lineage (SAP SD, Odoo, ERPNext), the codegen lineage, and the seam lineage — what selling keeps and rejects.
3. [Technology & the "why"](handbook/03-technology.md) — *Evaluator + Maintainer.* The stack, each choice with a rationale; why `rust_decimal` for money and `serde` as the cross-module contract.
4. [Architecture](handbook/04-architecture.md) — *Maintainer.* C4 view, the 4-layer shape + the custom seam, and a revenue invoice traced across the GL boundary.
5. [Maintainer Guide](handbook/05-maintainer-guide.md) — *Maintainer.* Schema-YAML SSoT, regeneration, the generated-vs-hand-written map, extending the write path and GL seam, release flow.
6. [Developer Guide](handbook/06-developer-guide.md) — *App developer.* Install → quickstart → a quotation-to-posted-invoice tutorial → recipes (the `GlPostSink`, order-to-cash, events) → config → troubleshooting.
7. [Contributing](handbook/07-contributing.md) — *Contributor.* Dev setup, commit/PR conventions, the golden oracle, boundary-integrity review checklist.
8. [Glossary](handbook/08-glossary.md) — *All.* One term, one meaning — selling domain first, framework mechanism second.
9. [Architecture Decision Records](handbook/adr/) — *Maintainer.* Framework ADRs (0001–0003) + selling-domain ADRs (boundary, GL seam, order status).

## Related, already-written docs

This handbook is the *narrative*. Reference sets live alongside it — link out, don't duplicate:

- **[Schema DSL reference](schema/README.md)** — the exact YAML grammar: [types](schema/TYPES.md), [model rules](schema/RULE_FORMAT_MODELS.md), [generation targets](schema/GENERATION.md), [error codes](schema/ERROR_CODES.md), [examples](schema/EXAMPLES.md). The *Reference* corner of Diátaxis; the handbook explains the *why*.
- **[Business flows](business-flows/README.md)** — one doc per flow (actors, preconditions, rules, postconditions), plus the [golden-case numeric oracle](business-flows/golden-cases.md), each linked to its executable BDD/test.
- **Selling-domain ADRs** — [boundary](adr/ADR-001-selling-boundary.md) · [GL seam](adr/ADR-002-gl-posting-seam.md) · [order status](adr/ADR-003-order-status-model.md).

## Conventions this handbook follows

- **Reader + mode named** at the top of every page.
- **Commands are real.** Every `metaphor …` command was run against `metaphor 0.2.0` while writing; stale skeleton-README commands are flagged with the working form.
- **Code wins over docs.** When a doc and the schema/code disagree, the schema YAML (the source of truth) wins — the doc is the bug.
