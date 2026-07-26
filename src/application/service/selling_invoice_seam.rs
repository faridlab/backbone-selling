//! The invoice seam: order-to-cash outbound to billing (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `build_invoice_request` is the OUTBOUND envelope asking billing to invoice the un-invoiced
//! remainder per line (`quantity − billed_qty`) — a composition layer maps it into billing's
//! `NewSalesInvoice` (adding A/R + revenue accounts) and posts the real revenue journal, retiring
//! `create_invoice_from_order` in the composed flow. `mark_invoiced` is the INBOUND handler for
//! billing's `SalesInvoicePosted` — bounded: it routes through a capacity-checked,
//! `FOR UPDATE`-serialized allocation capped at each line's `quantity`, and rejects `OverBilled`.
//! Without that, a racy/repeat `build_invoice_request` (billed_qty advances only at post time) or
//! a directly-raised invoice could push `billed_qty` past `quantity` — booking revenue beyond the
//! order while `recompute_order_status` (`billed_qty ≥ quantity`) silently masks it as `completed`.
//! Aggregate-by-item, fill in line order — correct even for duplicate-item orders. Status recompute
//! rides the shared `pub(super)` helper in
//! [`super::selling_invoice_post::SellingWriteService::recompute_order_status`].
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `SalesOrderRepository` / `SalesOrderItemRepository`.

use backbone_orm::company_scope;
use rust_decimal::Decimal;
use uuid::Uuid;

use super::selling_events::{InvoiceRequestEnvelope, InvoiceRequestLine, SellingEvent};
use super::selling_write_service::{SellingError, SellingWriteService};

impl SellingWriteService {
    /// Build the invoice request for a confirmed order (the order-to-cash mirror of
    /// `build_delivery_request`): asks billing to invoice only the **un-invoiced remainder**
    /// (`quantity − billed_qty`) per line, carrying the unit price. A composition layer maps the
    /// emitted `OrderInvoiced` envelope into billing's `NewSalesInvoice` (adding the A/R + revenue
    /// accounts) and posts the real revenue journal — so selling no longer owns invoicing or posts
    /// revenue itself (retiring `create_invoice_from_order` in the composed flow).
    pub async fn build_invoice_request(&self, order_id: Uuid) -> Result<InvoiceRequestEnvelope, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — see `build_delivery_request`. Read-only.
        let hdr = self.repos.orders.find_fulfillment_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        if hdr.status == "draft" {
            return Err(SellingError::NotDraft(order_id.to_string()));
        }
        let rows = self.repos.order_items.list_billing_remainders(&self.db_pool, order_id).await?;
        let lines: Vec<InvoiceRequestLine> = rows.iter().map(|r| InvoiceRequestLine {
            item_id: r.item_id,
            quantity: r.remaining,
            unit_price: r.unit_price,
        }).collect();
        let env = InvoiceRequestEnvelope {
            order_id,
            company_id: hdr.company_id,
            customer_id: hdr.customer_id,
            currency: hdr.currency,
            lines,
        };
        self.sink.publish(SellingEvent::OrderInvoiced(env.clone()));
        Ok(env)
    }

    /// Record that an order was invoiced (the inbound handler for billing's `SalesInvoicePosted`):
    /// advance `billed_qty` per item and recompute the order status. The order-to-cash mirror of
    /// buying's `mark_billed` (council 2026-07-05): **bounded** — it routes through a capacity-checked,
    /// `FOR UPDATE`-serialized allocation capped at each line's `quantity`, and **rejects** an over-bill
    /// (`OverBilled`). Without this, a racy/repeat `build_invoice_request` (billed_qty advances only at
    /// post time) or a directly-raised invoice could push `billed_qty` past `quantity` — booking revenue
    /// beyond the order while `recompute_order_status` (`billed_qty ≥ quantity`) silently masks it as
    /// `completed`. Serializing the *writer* (not just the upstream remainder) is what closes the race.
    /// Aggregate-by-item, fill in line order — correct even for duplicate-item orders.
    pub async fn mark_invoiced(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        billed: &[(Uuid, Decimal)],
    ) -> Result<(), SellingError> {
        // RLS scope (ADR-0008): company on the parameter — the allocation tx binds it explicitly
        // (`bind_company_on`), and the status recompute runs inside the scope. The inbound handler for
        // billing's `SalesInvoicePosted` passes the event's company; an event/job caller can no longer
        // forget to scope the `FOR UPDATE` reads inside `allocate_billed`.
        company_scope::with_company_scope(Some(company_id), async move {
            let mut tx = self.db_pool.begin().await?;
            company_scope::bind_company_on(&mut tx, company_id).await?;
            for (item_id, qty) in billed {
                self.allocate_billed(&mut tx, order_id, *item_id, *qty).await?;
            }
            tx.commit().await?;
            self.recompute_order_status(order_id).await?;
            Ok(())
        }).await
    }

    /// Fill `billed_qty` up to `quantity` across an item's order lines (`FOR UPDATE`, fill-in-order);
    /// reject when the requested qty exceeds total remaining capacity (`quantity − billed_qty`).
    ///
    /// The lock-read and the writes MUST share the caller's `tx` — that is what serializes concurrent
    /// billers; splitting them across connections would re-open the over-bill race.
    async fn allocate_billed(&self, tx: &mut sqlx::PgConnection, order_id: Uuid, item_id: Uuid, mut qty: Decimal) -> Result<(), SellingError> {
        let lines = self.repos.order_items.lock_billing_capacity(&mut *tx, order_id, item_id).await?;
        let total_cap: Decimal = lines.iter().map(|r| r.capacity).sum();
        if qty > total_cap {
            return Err(SellingError::OverBilled);
        }
        for line in &lines {
            if qty <= Decimal::ZERO { break; }
            let cap = line.capacity;
            if cap <= Decimal::ZERO { continue; }
            let take = if qty < cap { qty } else { cap };
            self.repos.order_items.add_billed_qty(&mut *tx, line.id, take).await?;
            qty -= take;
        }
        Ok(())
    }
}
