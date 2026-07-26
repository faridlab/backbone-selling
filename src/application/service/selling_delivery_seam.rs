//! The delivery seam: selling ↔ inventory (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `build_delivery_request` is the OUTBOUND envelope — a composition layer maps it into
//! inventory's `DeliveryRequested` (it durably stages the cross-module event in selling's outbox
//! before the in-proc publish so a crash between commit and publish cannot drop it). `mark_delivered`
//! is the INBOUND handler for inventory's `StockDelivered` — advances `delivered_qty` per item and
//! recomputes the order status via the shared `pub(super)` helper in
//! [`super::selling_invoice_post::SellingWriteService::recompute_order_status`].
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `SalesOrderRepository` / `SalesOrderItemRepository`.

use backbone_orm::company_scope;
use rust_decimal::Decimal;
use uuid::Uuid;

use super::selling_events::{DeliveryRequestEnvelope, DeliveryRequestLine, SellingEvent};
use super::selling_write_service::{SellingError, SellingWriteService};

impl SellingWriteService {
    /// Build the cross-module delivery request for a confirmed order (the envelope selling emits;
    /// a fulfillment/composition layer maps it into inventory's `DeliveryRequested`). Emits the
    /// `DeliveryRequested` domain event. Guard: the order must be confirmed (not draft/cancelled).
    pub async fn build_delivery_request(&self, order_id: Uuid) -> Result<DeliveryRequestEnvelope, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern: the reads ride the request-dedicated connection;
        // having read the order we bind ITS company onto the outbox transaction below.
        let hdr = self.repos.orders.find_fulfillment_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        if hdr.status == "draft" {
            return Err(SellingError::NotDraft(order_id.to_string())); // reuse: "not in a confirmable/deliverable state"
        }
        let rows = self.repos.order_items.list_delivery_remainders(&self.db_pool, order_id).await?;
        let lines: Vec<DeliveryRequestLine> = rows.iter().map(|r| DeliveryRequestLine {
            item_id: r.item_id,
            quantity: r.remaining,
        }).collect();
        let env = DeliveryRequestEnvelope {
            order_id,
            company_id: hdr.company_id,
            customer_id: hdr.customer_id,
            currency: hdr.currency,
            lines,
        };
        // Durably stage the cross-module event before the in-proc publish (outbox rollout plan, P1):
        // inventory SUBSCRIBES to DeliveryRequested to move stock + post COGS, so a crash between here and
        // the in-proc publish must not drop it. Staged in its own tx → the relay drains selling.outbox_events;
        // the in-proc publish stays as the fast path.
        let event = SellingEvent::DeliveryRequested(env.clone());
        let record = backbone_outbox::OutboxRecord::new(
            "DeliveryRequested", "SalesOrder", order_id.to_string(), env.company_id,
            serde_json::to_value(&event).map_err(|e| SellingError::Outbox(e.to_string()))?,
            chrono::Utc::now(),
        );
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, env.company_id).await?;
        backbone_outbox::outbox::stage(&mut *tx, "selling", &record)
            .await.map_err(|e| SellingError::Outbox(format!("stage: {e}")))?;
        tx.commit().await?;
        self.sink.publish(event);
        Ok(env)
    }

    /// Record a delivery against an order (the inbound handler for inventory's `StockDelivered`):
    /// advance `delivered_qty` per item and recompute the order status. Matches by `item_id`.
    pub async fn mark_delivered(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        deliveries: &[(Uuid, Decimal)],
    ) -> Result<(), SellingError> {
        // RLS scope (ADR-0008): company on the parameter — scope the delivered-qty writes + status
        // recompute so they run with `app.company_id` set. The inbound handler for inventory's
        // `StockDelivered` passes the event's company; an event/job caller can no longer forget to.
        company_scope::with_company_scope(Some(company_id), async move {
            for (item_id, qty) in deliveries {
                self.repos.order_items
                    .add_delivered_qty(&self.db_pool, order_id, *item_id, *qty)
                    .await?;
            }
            self.recompute_order_status(order_id).await?;
            Ok(())
        }).await
    }
}
