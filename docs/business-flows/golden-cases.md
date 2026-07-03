# Selling — Golden Cases (the numeric oracle)

Exact expected results, mirroring `tests/selling_golden_cases.rs`, `tests/integrity_probes.rs`, and
the cross-module seam in `tests/gl_posting_seam.rs`. Money is exact IDR (2 decimals, round-half-up).
Tax uses a single supplied `tax_rate` (real multi-rate tax is `backbone-tax`; see ADR-001).

## Write path (`tests/selling_golden_cases.rs`)

| Case | Input | Expected |
|------|-------|----------|
| **SGC-1** | invoice: 1 line qty 3 × 250,000, PPN 11% | subtotal `750,000`, tax `82,500.00`, total `832,500.00`; envelope balances with 3 lines. |
| **SGC-2** | invoice: 3 lines across 2 income accounts (100k+50k→A, 200k→B), no tax | 2 revenue credits `A=150,000`, `B=200,000`; A/R debit `350,000`. |
| **SGC-3** | invoice: 1 line, tax_rate 0, no PPN account | envelope has exactly 2 lines (Dr A/R + Cr Revenue); balanced. |
| **SGC-4** | invoice: subtotal 100.05 @ 11% | tax `11.01` (11.0055 half-up), total `111.06`; envelope still balances exactly. |
| **SGC-5** | line qty 2 × 100,000, discount 25,000 | subtotal `175,000`. |
| **SGC-6** | empty doc / discount>line / no revenue acct / tax w/o PPN acct / dup number | `empty_document` / `negative_quantity` / `missing_revenue_account` / `tax_account_missing` / `duplicate_number`. |
| **SGC-7** | quotation (10 × 100,000 @ 11%) → order → confirm | quotation total `1,110,000`; order → `to_bill` (ADR-003); re-confirm → `not_draft`. |

## Guarded route surface (`tests/integrity_probes.rs`)

| Case | Input via guarded routes | Expected |
|------|--------------------------|----------|
| **IGC-1** | `POST /sales-invoices/bulk` (generic) | `405/404` — generic bulk not exposed. |
| **IGC-2** | `DELETE /sales-invoices/{id}` (generic) | `405/404` — generic delete not exposed. |
| **IGC-3** | `POST /sales-invoices` well-formed | `201`. |
| **IGC-4** | `POST /sales-invoices` with `lines:[]` | `422 empty_document`. |
| **IGC-5** | `POST /sales-invoices` tax>0, no PPN account | `422 tax_account_missing`. |

## GL-posting seam (`tests/gl_posting_seam.rs`) — selling → the REAL accounting ledger

| Case | Input | Expected |
|------|-------|----------|
| **SEAM-1** | post invoice: 1,000,000 + PPN 11% | balanced journal: `Dr A/R 1,110,000` (customer party) · `Cr Revenue 1,000,000` · `Cr PPN Output 110,000`; invoice → `posted`/`submitted`, `journal_id` + `accounting_post_id` set, `outstanding = 1,110,000`. |
| **SEAM-2** | post the same invoice twice | idempotent: **one** journal for the company; second call replays the recorded ids. |
| **SEAM-3** | A/R points at a non-postable header account | GL rejects `non_postable_account`; invoice → `failed`; **no** journal written. |

## Order-to-cash conversion (`tests/order_to_cash.rs`)

| Case | Input | Expected |
|------|-------|----------|
| **OTC-1** | Quotation (10×100,000 @11%) → accept → convert → confirm → invoice-from-order → post | quotation→`ordered`; order→`to_bill`; invoice links order (subtotal 1,000,000, tax 110,000); post advances `billed_qty`=10 and order→`completed`. |
| **OTC-2** | convert a non-accepted quotation | `422 quotation_not_accepted`. |

## Extension contract §5 (`tests/extension_contract.rs` + `scripts/regen_roundtrip.sh`)

| Case | Input | Expected |
|------|-------|----------|
| **EXT-1** | consumer `CreditWatchConsumer` (limit 5,000,000) subscribes to `SalesOrderConfirmed`; confirm a 1M then a 9M order | under-limit → no breach; over-limit → 1 breach recorded. Selling emits; the consumer decides. |
| **EXT-2** | confirm an order with no consumer wired | still confirms (`to_bill`) — the event surface is additive. |
| **regen round-trip** | `metaphor schema schema generate --force` then re-run | all 10 user-owned files byte-identical; consumer rule + seam still green — the consumer's rule **survives regen** (§5 clause 2). |

## Conventions
- Selling **emits** a balanced `AccountingPostEnvelope`; it never writes GL rows. The revenue
  post is `Dr A/R (total) · Cr Revenue (per income account) · Cr PPN Output (tax)`.
- `posting_state` (pending→posted/failed) is the GL reconciliation state, distinct from the invoice
  document `status`.
- Idempotency key = invoice id; a re-post never double-recognises revenue.
