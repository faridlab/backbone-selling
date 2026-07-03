# BRD — backbone-selling

> Business Requirements & Rules. Tier 2 · Supply Chain. Date: 2026-07-03. Pairs with the golden
> cases (`docs/business-flows/golden-cases.md`) — every rule below has a numeric/BDD oracle.

## 1. Actors & documents

- **Quotation** — a priced, time-boxed offer to a customer.
- **Sales Order** — the confirmed demand commitment.
- **Sales Invoice** — the billable, revenue-recognising document (emits the GL post).
- **Sales Team / Sales-Person Allocation** — commission attribution on an order.

## 2. Business rules (authoritative)

**BR-1 (server-side money).** `line_amount = round₂(qty × unit_price) − round₂(discount)`;
`subtotal = Σ line_amount`; `tax_amount = round₂(subtotal × tax_rate/100)`; `total = subtotal +
tax_amount`. Rounding is half-away-from-zero to 2 decimals (IDR). Clients never set computed fields.

**BR-2 (no empty/negative document).** A document must have ≥1 line; no line may have a negative
quantity, price, discount, or net amount. → `empty_document` / `negative_quantity`.

**BR-3 (invoice account completeness).** Every invoice line carries an income account
(`revenue_account_id`); if `tax_amount > 0` a `tax_output_account_id` is required. →
`missing_revenue_account` / `tax_account_missing`.

**BR-4 (unique document numbers).** `quotation_number` / `order_number` / `invoice_number` are unique
per module (soft-delete aware). → `duplicate_number`.

**BR-5 (IDR-only revenue).** A sales invoice must be IDR; a non-IDR post is refused
(`unsupported_currency`) and a DB `CHECK` blocks non-IDR invoices — until a multi-currency FX
contract exists (ADR-002).

**BR-6 (revenue posting).** On post, selling emits `Dr A/R = total` (with the **customer** as the
A/R subledger party) · `Cr Revenue` per income account (summed) · `Cr PPN Output = tax_amount`. The
envelope balances by construction (`Σ debit = Σ credit = total`).

**BR-7 (idempotent posting).** An invoice recognises revenue **once**. Re-posting returns the recorded
journal without a second GL entry (accounting dedupes on the invoice identity; proven concurrency-safe).

**BR-8 (posting reconciliation).** A confirmed post sets the invoice `posting_state=posted`,
`status=submitted`, records `journal_id`/`accounting_post_id`, and `outstanding_amount=total`. A GL
rejection surfaces the ledger's stable code and sets `posting_state=failed` (no journal).

**BR-9 (quotation lifecycle).** `draft → sent → accepted → ordered`; only an **accepted** quotation
may convert to an order (`quotation_not_accepted`). Conversion copies header + lines and links
`quotation_id`.

**BR-10 (order lifecycle — ADR-003).** `draft → to_bill` on confirm; `→ completed` once every line is
fully billed (`billed_qty ≥ quantity`). Delivery states (`to_deliver*`) are inventory-gated.

**BR-11 (billing watermarks).** Posting an invoice raised from an order advances each source order
line's `billed_qty` by the invoiced quantity; the order completes when fully billed.

**BR-12 (commission allocation).** Σ `allocated_pct` per order across sales-person allocations must be
≤ 100 (attribution, not enforced against payout here).

## 3. Events (business-visible)

`QuotationAccepted`, `SalesOrderConfirmed` (carries grand_total/currency for consumer credit checks),
`SalesInvoiceIssued`, `SalesInvoicePosted`. Consumers subscribe; selling never calls back.

## 4. Deferred (with reason)

Delivery/COGS (needs inventory), credit-limit enforcement (Tier 3 / consumer), product bundles,
installation notes, multi-currency, real multi-rate tax (backbone-tax), reversal-on-cancel.
