# ADR-002: The GL-posting seam — a serialized envelope + ACL adapter, never a code edge

**Status**: Accepted — **Applied 2026-07-03** (the marquee cross-module seam, proven end-to-end)
**Deciders**: Farid (owner), build session 2026-07-03
**Related**: `docs/erp/gl-posting-contract.md`, `docs/erp/extension-contract.md`, accounting ADR
(the inbound `PostingService` port), ADR-001

## Context

The whole decomposition thesis rests on one claim: *transactional modules stay independent of the
General Ledger because they emit a posting request instead of calling into it.* Until a producer
actually posted into `backbone-accounting` end-to-end, that was a claim, not a fact. Selling is the
first producer; this ADR records how the seam is built and proven.

The constraint: **no producer may import accounting, and accounting may import no producer** (0
horizontal Cargo edges in shipped libraries). Accounting's inbound port is
`PostingService::post(PostingRequest, …)` — a Rust type with no Serde derive, deliberately *not*
shareable as a wire type.

## Decision

1. **Selling emits a serialized `AccountingPostEnvelope`**, not an accounting type. The envelope
   mirrors the contract shape exactly (`idempotency_key`, company/branch, `source_*`,
   `posting_date`, currency, `posting_type`, balanced `lines[]`). It is defined in selling
   (`application::service::selling_gl`) and is `Serialize`/`Deserialize` — **the envelope is the
   versioned contract, not a shared struct.**
2. **The revenue post is built deterministically and always balanced.** `build_revenue_post`
   produces: `Dr A/R = total` (carrying the customer party for subledger aging) · `Cr Revenue`
   grouped/summed per income account (deterministic `BTreeMap` order) · `Cr PPN Output = tax_amount`
   (only when tax > 0). The builder refuses to emit an unbalanced envelope
   (`Σ debit == Σ credit == total`), so accounting's balance check can never be the first line of
   defense.
3. **Delivery crosses the boundary through a `GlPostSink` trait.** Selling calls
   `sink.post(&envelope).await`; the composing service (or, in the proof, the seam test) implements
   the sink over accounting's real `PostingService`, mapping envelope → `PostingRequest` — the ACL
   translation. Selling never names an accounting type.
4. **Idempotent + reconciled.** `idempotency_key = invoice_id`, so a replay returns accounting's
   original result (no double revenue). Selling short-circuits an already-`posted` invoice without
   re-emitting, and on a successful ack sets `posting_state=posted`, `status=submitted`,
   `journal_id`, `accounting_post_id`, `posted_at`, `outstanding_amount=total`. A rejection surfaces
   accounting's **stable error code** and sets `posting_state=failed` (no partial write in the GL).
5. **The proof’s only edge to accounting is a dev-dependency.** `tests/gl_posting_seam.rs` drives
   the real `PostingService`; `cargo tree -e normal -i backbone-accounting` is **empty**. Both
   modules keep their own Postgres schema (`selling.*`, `accounting.*`) in one database, so one
   connection serves both — exactly as a composed service runs them.

6. **Revenue posts are IDR-only until FX is designed.** The GL is kept in the company base
   currency and the envelope carries no `exchange_rate`. `build_revenue_post` refuses a non-IDR
   post (`unsupported_currency`, 422) and a protected `CHECK (currency = 'IDR')` on
   `selling.sales_invoices` blocks a non-IDR invoice at creation — so a foreign invoice can never
   silently book face-value amounts into an IDR ledger (council 2026-07-03). Multi-currency is a
   deferred, separately-designed contract.

## Consequences

- **Concurrency-safe, verified (not assumed).** The council probed whether two concurrent posts of
  one invoice could double the ledger. They cannot: accounting's partial unique index
  `(company, source_type, source_id, posting_type) WHERE posted` is the arbiter, the adapter sets
  `source_id = invoice_id`, and `tests/gl_posting_seam::concurrent_double_post_yields_one_journal`
  shows exactly **one** journal (the loser rolls back; both callers get the winner's ids). Selling's
  local short-circuit is a fast path, not the guard; the `idempotency_key` field is redundant with
  `source_id` for this seam (see the field docstring).
- **Proven, not asserted.** `tests/gl_posting_seam.rs` shows a 1,000,000 + 11% PPN invoice landing
  a balanced 3-line journal in the real ledger (Dr A/R 1,110,000 w/ customer party · Cr Revenue
  1,000,000 · Cr PPN Output 110,000), a second post replaying idempotently (one journal), and a
  non-postable account producing a `failed` invoice with no journal.
- This is the **reference implementation of the extension contract**: it survives a regen of both
  modules (the envelope + sink live in user-owned files; the accounting adapter lives in the
  consumer/test). Buying → Inventory (COGS) and Billing → GL will reuse it verbatim.
- Residual / parking lot: async/durable posting (scheduled→posted with retry) instead of the
  synchronous ack; consuming accounting's `AccountingPostFailed` event for automatic retry;
  reversal on invoice cancellation (emit `posting_type=reversal`); real `backbone-tax` line
  computation feeding `lines[]`.
