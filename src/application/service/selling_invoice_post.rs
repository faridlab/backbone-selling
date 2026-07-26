//! Posting a sales invoice to the GL + advancing the source order's billing watermarks
//! (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `build_revenue_post` is the pure + deterministic envelope builder (Dr A/R with customer party ·
//! Cr Revenue grouped per income account · Cr PPN Output iff tax > 0). `post_sales_invoice` emits
//! it through the `GlPostSink`, reconciles the invoice from the ack (idempotent — a second call on
//! an already-posted invoice returns the recorded ids without re-emitting), and advances the
//! source order's billed watermarks.
//!
//! `recompute_order_status` is the shared watermark → status rollup (ADR-003: `completed` iff
//! every line is fully billed AND fully delivered). It is `pub(super)` because the delivery seam
//! (`mark_delivered`) and the invoice seam (`mark_invoiced`) also drive it after advancing their
//! own watermarks.
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `SalesInvoiceRepository` / `SalesInvoiceItemRepository` / `SalesOrderItemRepository` /
//! `SalesOrderRepository`.

use rust_decimal::Decimal;
use std::collections::BTreeMap;
use uuid::Uuid;

use super::selling_events::{SalesInvoicePosted, SellingEvent};
use super::selling_gl::{AccountingPostEnvelope, GlPostLine, GlPostSink};
use super::selling_write_service::{PostOutcome, SellingError, SellingWriteService};

impl SellingWriteService {
    /// Build the balanced revenue posting envelope for an invoice: Dr A/R (total, with customer
    /// party) · Cr Revenue (per income account, summed) · Cr PPN Output (tax_amount, if any).
    /// Pure + deterministic — the golden oracle asserts these lines directly.
    pub async fn build_revenue_post(&self, invoice_id: Uuid) -> Result<AccountingPostEnvelope, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — see `convert_quotation_to_order` in `selling_order`.
        let inv = self.repos.invoices.find_post_source(&self.db_pool, invoice_id).await?
            .ok_or(SellingError::InvoiceNotFound(invoice_id))?;

        let company_id: Uuid = inv.company_id;
        let branch_id: Option<Uuid> = inv.branch_id;
        let customer_id: Uuid = inv.customer_id;
        let invoice_number: String = inv.invoice_number;
        let invoice_date: chrono::NaiveDate = inv.invoice_date;
        let currency: String = inv.currency;
        let tax_amount: Decimal = inv.tax_amount;
        let total: Decimal = inv.total;
        let receivable_account_id: Uuid = inv.receivable_account_id;
        let tax_output_account_id: Option<Uuid> = inv.tax_output_account_id;

        // The GL is kept in the company base currency (IDR) and the envelope carries no
        // exchange_rate (multi-currency is a deferred, separately-designed contract — council
        // 2026-07-03). Refuse to emit a non-IDR post rather than silently booking foreign
        // face-value amounts into an IDR ledger. Backed by a CHECK on selling.sales_invoices.
        if currency != "IDR" {
            return Err(SellingError::UnsupportedCurrency(currency));
        }

        // Credit revenue grouped by income account (BTreeMap → deterministic line order).
        let rows = self.repos.invoice_items.list_revenue_lines(&self.db_pool, invoice_id).await?;
        if rows.is_empty() {
            return Err(SellingError::EmptyDocument);
        }
        let mut revenue: BTreeMap<Uuid, Decimal> = BTreeMap::new();
        for r in &rows {
            *revenue.entry(r.revenue_account_id).or_insert(Decimal::ZERO) += r.line_amount;
        }

        let mut lines: Vec<GlPostLine> = Vec::new();
        // Dr A/R (control) — carries the customer party for subledger aging.
        lines.push(
            GlPostLine::debit(receivable_account_id, total)
                .with_party("customer", customer_id)
                .with_description(format!("A/R {invoice_number}")),
        );
        // Cr Revenue per income account.
        for (acct, amt) in &revenue {
            lines.push(GlPostLine::credit(*acct, *amt).with_description("Revenue"));
        }
        // Cr PPN Output.
        if tax_amount > Decimal::ZERO {
            let tax_acct = tax_output_account_id.ok_or(SellingError::TaxAccountMissing)?;
            lines.push(GlPostLine::credit(tax_acct, tax_amount).with_description("PPN Output"));
        }

        let envelope = AccountingPostEnvelope {
            idempotency_key: invoice_id.to_string(),
            company_id,
            branch_id,
            source_type: "order".into(),
            source_id: invoice_id,
            source_reference: Some(invoice_number),
            posting_date: invoice_date,
            currency,
            posting_type: "original".into(),
            reverses_post_id: None,
            description: Some("Sales invoice revenue".into()),
            lines,
        };
        // Defensive: never emit an unbalanced envelope (would be rejected downstream anyway).
        if !envelope.is_balanced() {
            return Err(SellingError::UnbalancedPost);
        }
        Ok(envelope)
    }

    /// Post an invoice's revenue to the GL through `sink`, then reconcile the invoice from the ack.
    /// Idempotent: a second call on an already-posted invoice returns the recorded ids without
    /// re-emitting. Guarded: only a `draft`/`pending` invoice is posted.
    pub async fn post_sales_invoice(
        &self,
        invoice_id: Uuid,
        sink: &dyn GlPostSink,
    ) -> Result<PostOutcome, SellingError> {
        // Idempotency short-circuit: already posted → return the recorded ids, no re-emit.
        // RLS scope (ADR-0008), ID-only pattern: identified by the invoice id alone. Under HTTP the
        // request-dedicated connection carries the scope. When driven by an EVENT or a job, the caller
        // must wrap this in `with_company_scope(Some(event.company_id))` — otherwise these reads fail
        // closed.
        let existing = self.repos.invoices.find_posting_state(&self.db_pool, invoice_id).await?
            .ok_or(SellingError::InvoiceNotFound(invoice_id))?;
        if existing.posting_state == "posted" {
            if let (Some(j), Some(p)) = (existing.journal_id, existing.accounting_post_id) {
                return Ok(PostOutcome { invoice_id, post_id: p, journal_id: j, idempotent_reuse: true });
            }
        }

        let envelope = self.build_revenue_post(invoice_id).await?;

        // Idempotency note: `envelope.source_id == invoice_id` is the identity accounting dedupes
        // on (its partial unique index on `(company, source_type, source_id, posting_type) WHERE
        // posted`). That index is the authoritative arbiter — two concurrent posts of one invoice
        // yield exactly ONE journal (proven by `gl_posting_seam::concurrent_double_post_*`), because
        // accounting rolls back the loser and returns the winner's ids to both callers. The local
        // guards here (the posted-short-circuit above + the `posting_state <> 'posted'` clause below)
        // are defense-in-depth so selling is self-consistent even if a downstream ever weakened.
        match sink.post(&envelope).await {
            Ok(ack) => {
                self.repos.invoices
                    .reconcile_posted(&self.db_pool, invoice_id, ack.journal_id, ack.post_id)
                    .await?;

                // Advance the source order's billed watermarks (only for a fresh post) and close it
                // out when fully billed. Each invoice line carries its `sales_order_item_id`.
                if !ack.idempotent_reuse {
                    self.advance_billing_watermarks(invoice_id).await?;
                }

                // Read total for the event, then publish SalesInvoicePosted.
                let total: Decimal = self.repos.invoices.fetch_total(&self.db_pool, invoice_id).await?;
                self.sink.publish(SellingEvent::SalesInvoicePosted(SalesInvoicePosted {
                    invoice_id,
                    company_id: envelope.company_id,
                    journal_id: ack.journal_id,
                    post_id: ack.post_id,
                    total,
                }));

                Ok(PostOutcome {
                    invoice_id,
                    post_id: ack.post_id,
                    journal_id: ack.journal_id,
                    idempotent_reuse: ack.idempotent_reuse,
                })
            }
            Err(rej) => {
                // Record the failure so the invoice reflects the rejected post (audit/retry).
                let _ = self.repos.invoices.mark_post_failed(&self.db_pool, invoice_id).await;
                Err(SellingError::GlRejected { code: rej.code, message: rej.message })
            }
        }
    }

    /// For each invoice line linked to a sales-order line, add the invoiced quantity to that SO
    /// line's `billed_qty`; then recompute the order status. No-op for a direct invoice.
    async fn advance_billing_watermarks(&self, invoice_id: Uuid) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — inherits the caller's scope (`post_sales_invoice`).
        // The repo statement is scoped through the INVOICE (its `sales_invoice_items` subquery), not
        // the order — that scoping is deliberate and unchanged.
        self.repos.order_items.advance_billed_from_invoice(&self.db_pool, invoice_id).await?;

        let order_id: Option<Uuid> =
            self.repos.invoices.fetch_sales_order_id(&self.db_pool, invoice_id).await?;
        if let Some(oid) = order_id {
            self.recompute_order_status(oid).await?;
        }
        Ok(())
    }

    /// Recompute an order's status from its two watermarks (ADR-003): `completed` iff every line is
    /// fully billed AND fully delivered; else `to_deliver` (billed, awaiting delivery) / `to_bill`
    /// (delivered, awaiting billing) / `to_deliver_and_bill` (awaiting both). Never touches a
    /// draft/closed/cancelled order.
    ///
    /// `pub(super)` because the delivery seam (`mark_delivered`) and the invoice seam
    /// (`mark_invoiced`) also drive this after advancing their own watermarks — the helper is the
    /// single watermark → status rollup so the three callers can never drift.
    pub(super) async fn recompute_order_status(&self, order_id: Uuid) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — inherits the caller's scope.
        let row = self.repos.order_items.watermark_rollup(&self.db_pool, order_id).await?;
        let next = match (row.billed_all.unwrap_or(false), row.delivered_all.unwrap_or(false)) {
            (true, true) => "completed",
            (true, false) => "to_deliver",
            (false, true) => "to_bill",
            (false, false) => "to_deliver_and_bill",
        };
        // Only advance an in-flight (confirmed) order; leave draft/closed/cancelled alone — the
        // repo statement's `status = ANY(...)` gate is what enforces that.
        self.repos.orders.advance_status(&self.db_pool, order_id, next).await?;
        Ok(())
    }
}
