//! The quotation lifecycle: create + accept (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`]:
//! `create_quotation` prices the document server-side (2dp half-up) and writes the header + lines
//! as ONE transaction; `accept_quotation` is the gated draft/sent → accepted flip. An accepted
//! quotation is the precondition for
//! [`super::selling_order::SellingWriteService::convert_quotation_to_order`].
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `QuotationRepository` / `QuotationItemRepository`, whose header+lines methods take THIS
//! service's transaction so a quotation is never half-written.

use backbone_orm::company_scope;
use uuid::Uuid;

use crate::infrastructure::persistence::{NewQuotationItemRow, NewQuotationRow};

use super::selling_events::{QuotationAccepted, SellingEvent};
use super::selling_write_service::{is_dup, price_document, NewQuotation, SellingError, SellingWriteService};

impl SellingWriteService {
    pub async fn create_quotation(&self, q: NewQuotation) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&q.lines, q.tax_rate)?;
        let id = Uuid::new_v4();
        let currency = q.currency.unwrap_or_else(|| "IDR".into());
        // RLS scope (ADR-0008): the header+lines insert runs in ONE transaction whose connection is
        // bound to this document's company, so every write is fenced by `app.company_id`. The explicit
        // `company_id` binds below stay as defense-in-depth.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, q.company_id).await?;
        let r = self.repos.quotations.insert_draft(&mut tx, &NewQuotationRow {
            id,
            quotation_number: &q.quotation_number,
            company_id: q.company_id,
            branch_id: q.branch_id,
            customer_id: q.customer_id,
            quotation_date: q.quotation_date,
            valid_until: q.valid_until,
            currency: &currency,
            subtotal,
            tax_rate: q.tax_rate,
            tax_amount,
            total,
            notes: q.notes.as_deref(),
        }).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(q.quotation_number) } else { e.into() });
        }
        for p in &priced {
            self.repos.quotation_items.insert_line(&mut tx, &NewQuotationItemRow {
                id: Uuid::new_v4(),
                quotation_id: id,
                company_id: q.company_id,
                item_id: p.item_id,
                description: p.description.as_deref(),
                quantity: p.quantity,
                unit_price: p.unit_price,
                line_discount: p.line_discount,
                line_amount: p.line_amount,
            }).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Accept a quotation (draft/sent → accepted); emits `QuotationAccepted`. Only an accepted
    /// quotation may be converted to a sales order.
    ///
    /// `company_id` scopes the lookup for the same reason as
    /// [`Self::confirm_sales_order`]: the caller's tenant must own the row, not merely be
    /// authenticated.
    pub async fn accept_quotation(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), SellingError> {
        // RLS scope (ADR-0008): company on the parameter — same shape as `confirm_sales_order`.
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.accept(&self.db_pool, quotation_id, company_id),
        ).await?;
        let row = row.ok_or_else(|| SellingError::NotDraft(quotation_id.to_string()))?;
        self.sink.publish(SellingEvent::QuotationAccepted(QuotationAccepted {
            quotation_id,
            company_id: row.company_id,
            customer_id: row.customer_id,
        }));
        Ok(())
    }
}
