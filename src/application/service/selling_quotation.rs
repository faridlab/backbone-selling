//! The quotation lifecycle: create (+ template stamping) + the state machine (hand-authored,
//! user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`]:
//! `create_quotation` prices the document server-side (2dp half-up) and writes the header + lines
//! as ONE transaction, stamping the caller's template (validity window + default notes) when the
//! caller supplied none; `accept_quotation` is the gated draft/sent → accepted flip; `send` /
//! `reject` / `cancel` / `re_draft` are the quotation state machine — each a guarded single-statement
//! flip whose `WHERE` clause IS the guard, so a wrong-state or wrong-tenant id is refused without
//! leaking whether the id exists. An accepted quotation is the precondition for
//! [`super::selling_order::SellingWriteService::convert_quotation_to_order`].
//!
//! Guard table (Odoo sale.order semantics, adapted):
//!
//! | verb     | from                       | to        | refused with                |
//! |----------|----------------------------|-----------|-----------------------------|
//! | send     | draft                      | sent      | `invalid_transition`        |
//! | accept   | draft, sent                | accepted  | `invalid_transition`        |
//! | reject   | sent                       | rejected  | `invalid_transition`        |
//! | cancel   | draft, sent, accepted      | cancelled | `invalid_transition`; from `ordered` → `quotation_ordered` |
//! | re_draft | sent, cancelled, rejected  | draft     | `invalid_transition` (never from `ordered`) |
//!
//! `ordered` is a one-way door: a confirmed order must never be orphaned by resetting or
//! cancelling its source quotation.
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `QuotationRepository` / `QuotationItemRepository` / `QuotationTemplateRepository`, whose
//! header+lines methods take THIS service's transaction so a quotation is never half-written.

use backbone_orm::company_scope;
use uuid::Uuid;

use crate::infrastructure::persistence::{
    NewQuotationItemRow, NewQuotationRow, NewQuotationTemplateRow, QuotationTemplateRow,
};

use super::selling_events::{
    QuotationAccepted, QuotationCancelled, QuotationReDrafted, QuotationRejected, QuotationSent,
    SellingEvent,
};
use super::selling_write_service::{is_dup, price_document, NewQuotation, SellingError, SellingWriteService};

impl SellingWriteService {
    pub async fn create_quotation(&self, q: NewQuotation) -> Result<Uuid, SellingError> {
        // Template stamping (BEFORE the write): the caller's own values always win; the template
        // fills valid_until (quotation_date + validity_days) and notes only where absent. The
        // template itself is not persisted on the quotation — its effects are stamped at create.
        let mut valid_until = q.valid_until;
        let mut notes = q.notes.clone();
        if let Some(template_id) = q.template_id {
            let t = company_scope::with_company_scope(
                Some(q.company_id),
                self.repos.templates.find_template(&self.db_pool, template_id, q.company_id),
            ).await?
                .ok_or(SellingError::TemplateNotFound(template_id))?;
            valid_until = valid_until
                .or_else(|| q.quotation_date.checked_add_signed(chrono::Duration::days(t.validity_days as i64)));
            notes = notes.or(t.default_notes);
        }

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
            valid_until,
            currency: &currency,
            subtotal,
            tax_rate: q.tax_rate,
            tax_amount,
            total,
            notes: notes.as_deref(),
            opportunity_id: q.opportunity_id,
            status_reason: None,
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
                invoice_policy: &p.invoice_policy.to_string(),
                is_downpayment: p.is_downpayment,
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
        // A refused guard is wrong-state OR wrong-tenant OR absent — classify only after the
        // refusal, exactly like the other machine verbs: the scoped re-read decides between
        // `invalid_transition` (422, the quotation is ours but not acceptable) and
        // `quotation_not_found` (404, a foreign or unknown id).
        let row = match row {
            Some(r) => r,
            None => return Err(self.refuse_quotation_transition("accept", quotation_id, company_id).await),
        };
        self.sink.publish(SellingEvent::QuotationAccepted(QuotationAccepted {
            quotation_id,
            company_id: row.company_id,
            customer_id: row.customer_id,
        }));
        Ok(())
    }

    /// Send a quotation to the customer (draft → sent); emits `QuotationSent`. Any other source
    /// state is refused with `invalid_transition` (a loud 422, never a silent no-op).
    pub async fn send_quotation(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), SellingError> {
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.send(&self.db_pool, quotation_id, company_id),
        ).await?;
        let row = match row {
            Some(r) => r,
            None => return Err(self.refuse_quotation_transition("send", quotation_id, company_id).await),
        };
        self.sink.publish(SellingEvent::QuotationSent(QuotationSent {
            quotation_id,
            company_id: row.company_id,
        }));
        Ok(())
    }

    /// Record the customer's decline (sent → rejected), persisting the optional reason; emits
    /// `QuotationRejected`.
    pub async fn reject_quotation(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
        reason: Option<String>,
    ) -> Result<(), SellingError> {
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.reject(&self.db_pool, quotation_id, company_id, reason.as_deref()),
        ).await?;
        let row = match row {
            Some(r) => r,
            None => return Err(self.refuse_quotation_transition("reject", quotation_id, company_id).await),
        };
        self.sink.publish(SellingEvent::QuotationRejected(QuotationRejected {
            quotation_id,
            company_id: row.company_id,
            reason,
        }));
        Ok(())
    }

    /// Withdraw a quotation (draft/sent/accepted → cancelled), persisting the optional reason;
    /// emits `QuotationCancelled`. Refused once `ordered` — an order was derived from this
    /// quotation and must never be orphaned (`quotation_ordered`).
    pub async fn cancel_quotation(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
        reason: Option<String>,
    ) -> Result<(), SellingError> {
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.cancel(&self.db_pool, quotation_id, company_id, reason.as_deref()),
        ).await?;
        let row = match row {
            Some(r) => r,
            None => return Err(self.refuse_quotation_cancel(quotation_id, company_id).await),
        };
        self.sink.publish(SellingEvent::QuotationCancelled(QuotationCancelled {
            quotation_id,
            company_id: row.company_id,
            reason,
        }));
        Ok(())
    }

    /// Return a quotation to draft for re-editing (sent/cancelled/rejected → draft), clearing any
    /// recorded reason; emits `QuotationReDrafted`. Never possible from `ordered`.
    pub async fn redraft_quotation(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), SellingError> {
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.redraft(&self.db_pool, quotation_id, company_id),
        ).await?;
        let row = match row {
            Some(r) => r,
            None => return Err(self.refuse_quotation_transition("re-draft", quotation_id, company_id).await),
        };
        self.sink.publish(SellingEvent::QuotationReDrafted(QuotationReDrafted {
            quotation_id,
            company_id: row.company_id,
        }));
        Ok(())
    }

    // -- refusal classification ---------------------------------------------------------------
    //
    // A failed guarded flip means wrong-state, wrong-tenant, or absent — indistinguishable by the
    // guarded statement alone (that indistinguishability is what avoids the existence leak). The
    // classification read runs only AFTER a refusal, to produce the precise error code.

    async fn refuse_quotation_transition(
        &self,
        verb: &str,
        quotation_id: Uuid,
        company_id: Uuid,
    ) -> SellingError {
        let st = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.find_status(&self.db_pool, quotation_id, company_id),
        ).await;
        match st {
            Ok(Some(row)) => SellingError::InvalidTransition { verb: verb.into(), current: row.status },
            _ => SellingError::QuotationNotFound(quotation_id),
        }
    }

    async fn refuse_quotation_cancel(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
    ) -> SellingError {
        let st = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.find_status(&self.db_pool, quotation_id, company_id),
        ).await;
        match st {
            Ok(Some(row)) if row.status == "ordered" => SellingError::QuotationOrdered(quotation_id),
            Ok(Some(row)) => SellingError::InvalidTransition { verb: "cancel".into(), current: row.status },
            _ => SellingError::QuotationNotFound(quotation_id),
        }
    }

    // -- quotation templates (per-tenant master data; the guarded route's write surface) --------

    /// Create a quotation template. The (company_id, name) unique index turns a duplicate name
    /// into `TemplateDuplicate` (refuse loudly, never silently merge).
    pub async fn create_quotation_template(
        &self,
        company_id: Uuid,
        name: &str,
        validity_days: i32,
        default_notes: Option<&str>,
    ) -> Result<Uuid, SellingError> {
        let id = Uuid::new_v4();
        let r = company_scope::with_company_scope(
            Some(company_id),
            self.repos.templates.insert_template(&self.db_pool, &NewQuotationTemplateRow {
                id,
                company_id,
                name,
                validity_days,
                default_notes,
            }),
        ).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::TemplateDuplicate(name.to_string()) } else { e.into() });
        }
        Ok(id)
    }

    /// List the caller's company's quotation templates (name order).
    pub async fn list_quotation_templates(
        &self,
        company_id: Uuid,
    ) -> Result<Vec<QuotationTemplateRow>, SellingError> {
        Ok(company_scope::with_company_scope(
            Some(company_id),
            self.repos.templates.list_templates(&self.db_pool, company_id),
        ).await?)
    }
}
