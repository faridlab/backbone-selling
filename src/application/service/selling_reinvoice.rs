//! The expense-reinvoice link (hand-authored, user-owned) — selling's side of the
//! rebill-expenses-to-the-customer seam.
//!
//! SELLING holds the association (an `ExpenseReinvoiceLink` row); the expense itself belongs to
//! backbone-expenses. `expense_id` is taken ON FAITH — the `opportunity_id` posture: there is no
//! cross-module key and no cargo edge, so THE HOST validates that the expense exists, belongs
//! to the same company, and is in a postable state before calling attach. Selling cannot verify
//! any of that; a host that skips validation could attach a foreign id, and that obligation is
//! documented here as the seam's contract.
//!
//! ## Host billing adapter contract (the pull model)
//!
//! No events, no outbox, no push: the adapter PULLS. To rebill an order's expenses it:
//!
//! 1. before building a customer invoice for order O, calls `list_expense_reinvoices(O)` and
//!    filters `state == "pending"`;
//! 2. adds invoice lines totaling `Σ pending amounts` in its own billing request shape (the
//!    line label/description comes from the expense through the host's expenses adapter —
//!    selling stores no label; the amount is in the ORDER's currency, and the host validates
//!    expense-currency compatibility, single-currency v1);
//! 3. after billing acks the post, calls `mark_expense_reinvoice_invoiced` per link.
//!
//! Selling exports ONLY the three verbs below — no billing types leak in, no cargo edge.
//!
//! The double-bill guard is the partial unique index on `(order_id, expense_id)` over live rows
//! (a soft-deleted link CAN be re-attached). Under a composed tenancy decorator (ADR-0029) the
//! decorator installs the pending-queue listing index (org-unit leading) and fences every read;
//! on an undecorated deployment the same statements run unfenced.
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `ExpenseReinvoiceLinkRepository` / `SalesOrderRepository`.

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::infrastructure::persistence::NewExpenseReinvoiceLinkRow;

use super::selling_write_service::{
    is_dup, relay_ambient_scope, SellingError, SellingWriteService,
};

impl SellingWriteService {
    /// Attach a customer-rebillable expense to an order (state starts `pending`). Attaching to
    /// a DRAFT order is allowed — estimating a quote-era charge onto the order before confirm is
    /// normal. Refusals: unknown order (`OrderNotFound`; under a composed decorator a foreign
    /// order is indistinguishable, which is the point), cancelled order (`InvalidTransition`),
    /// non-positive amount (`InvalidReinvoiceAmount`), a live duplicate `(order, expense)` link
    /// (`DuplicateReinvoice` — pre-read first, the partial unique index backs the race).
    pub async fn attach_expense_reinvoice(
        &self,
        order_id: Uuid,
        expense_id: Uuid,
        amount: Decimal,
    ) -> Result<Uuid, SellingError> {
        if amount <= Decimal::ZERO {
            return Err(SellingError::InvalidReinvoiceAmount);
        }
        // The order must exist and not be cancelled.
        let why = self.repos.orders.find_cancel_refusal(&self.db_pool, order_id).await?;
        match why {
            None => return Err(SellingError::OrderNotFound(order_id)),
            Some(r) if r.status == "cancelled" => {
                return Err(SellingError::InvalidTransition { verb: "attach_expense_reinvoice".into(), current: r.status });
            }
            Some(_) => {}
        }
        // Duplicate pre-read (the partial unique index backs the race below).
        let dup = self.repos.reinvoices.find_live_link(&self.db_pool, order_id, expense_id).await?;
        if dup.is_some() {
            return Err(SellingError::DuplicateReinvoice);
        }

        let id = Uuid::new_v4();
        let mut tx = self.db_pool.begin().await?;
        relay_ambient_scope(&mut tx).await?;
        let r = self.repos.reinvoices.insert_link(&mut tx, &NewExpenseReinvoiceLinkRow {
            id,
            order_id,
            expense_id,
            amount,
        }).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateReinvoice } else { e.into() });
        }
        tx.commit().await?;
        Ok(id)
    }

    /// List an order's links — the host billing adapter's pull read. ID-only like the verb
    /// siblings: under a composed tenancy decorator (ADR-0029) the request's fence scopes the
    /// order-header probe, so a foreign order id is a plain `OrderNotFound` and another tenant's
    /// links are never reachable; an undecorated deployment lists all.
    pub async fn list_expense_reinvoices(
        &self,
        order_id: Uuid,
    ) -> Result<Vec<ExpenseReinvoiceDto>, SellingError> {
        // Existence probe: the order must exist (None ⇒ not found, no leak).
        let why = self.repos.orders.find_cancel_refusal(&self.db_pool, order_id).await?;
        if why.is_none() {
            return Err(SellingError::OrderNotFound(order_id));
        }
        let rows = self.repos.reinvoices.list_links_for_order(&self.db_pool, order_id).await?;
        Ok(rows.into_iter().map(|l| ExpenseReinvoiceDto {
            id: l.id,
            order_id: l.order_id,
            expense_id: l.expense_id,
            amount: l.amount,
            state: l.state,
            created_at: l.created_at,
        }).collect())
    }

    /// Flip a link pending → invoiced — called by the host billing adapter AFTER its invoice
    /// post for the order acked. NOT idempotent-by-silence: a double mark is a LOUD refusal
    /// (`InvalidTransition`, machine-verb posture), so a billing retry that re-marks an already
    /// invoiced link surfaces instead of silently passing. Unknown link ⇒ `ReinvoiceNotFound`.
    pub async fn mark_expense_reinvoice_invoiced(
        &self,
        link_id: Uuid,
    ) -> Result<(), SellingError> {
        let updated = self.repos.reinvoices.mark_invoiced(&self.db_pool, link_id).await?;
        if !updated {
            // The guarded statement refused — classify why (only after a refusal).
            let st = self.repos.reinvoices.find_link_state(&self.db_pool, link_id).await?;
            return Err(match st {
                None => SellingError::ReinvoiceNotFound(link_id),
                Some(state) => SellingError::InvalidTransition { verb: "mark_invoiced".into(), current: state },
            });
        }
        Ok(())
    }
}

/// One link as the billing-adapter pull read returns it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpenseReinvoiceDto {
    pub id: uuid::Uuid,
    pub order_id: uuid::Uuid,
    pub expense_id: uuid::Uuid,
    pub amount: rust_decimal::Decimal,
    /// "pending" | "invoiced"
    pub state: String,
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}
