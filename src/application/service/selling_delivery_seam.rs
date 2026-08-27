//! The delivery seam: selling ↔ inventory (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `build_delivery_request` is the OUTBOUND envelope — a composition layer maps it into
//! inventory's `DeliveryRequested` (it durably stages the cross-module event in selling's outbox
//! before the in-proc publish so a crash between commit and publish cannot drop it). `mark_delivered`
//! is the INBOUND handler for inventory's `StockDelivered` — **bounded** (council 2026-07-27): it
//! routes through a capacity-checked, `FOR UPDATE`-serialized allocation capped at each line's
//! `quantity`, and rejects `OverDelivered`, then recomputes the order status via the shared
//! `pub(super)` helper in [`super::selling_invoice_post::SellingWriteService::recompute_order_status`].
//!
//! The sale_stock confirm engine adds the MOVE-BACKED half of the inbound direction:
//! `order_delivery_view` reconstructs each line's delivered quantity from the stock engine's
//! done moves through the [`super::selling_stock_fulfillment`] port — gross outgoing minus the
//! returns flagged to-refund (an exchanged return ships a replacement and does not reduce the
//! delivered commitment) — and `sync_delivered_from_moves` reconciles the stored
//! `delivered_qty` watermark to that reconstruction. The two inbound paths coexist deliberately:
//! `mark_delivered` advances an event's figure under the over-delivery cap, while the
//! reconstruction REPLACES the watermark with the physical truth (a refund-shaped return can
//! legitimately lower it); the moves are the truth, the watermark is the cached projection.
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `SalesOrderRepository` / `SalesOrderItemRepository`.

use backbone_orm::company_scope;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::selling_events::{DeliveryRequestEnvelope, DeliveryRequestLine, SellingEvent};
use super::selling_stock_fulfillment::{
    DeliveredQtyLineRef, DeliveredQtyRequest, MoveDeliveryFigures, StockFulfillmentPort,
};
use super::selling_write_service::{SellingError, SellingWriteService};

// --- the move-backed delivery read model ----------------------------------------

/// One line of the order delivery view: the ordered quantity, the STORED watermark, and —
/// when the stock engine reported move-backed figures for the line — the raw figures plus
/// the return-adjusted reconstruction selling derives from them. `move_*` fields are `None`
/// when no figure exists for the line (no stock engine composed, or no moves were ever
/// minted): absence, never zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SalesOrderDeliveryLineDto {
    pub line_id: Uuid,
    pub item_id: Uuid,
    pub quantity: Decimal,
    /// The stored `delivered_qty` watermark (what `mark_delivered` / the sync last wrote).
    pub stored_delivered_qty: Decimal,
    /// DONE outgoing moves, gross.
    pub move_delivered_qty: Option<Decimal>,
    /// DONE incoming returns, all of them.
    pub move_returned_qty: Option<Decimal>,
    /// The returned subset flagged to-refund.
    pub move_to_refund_qty: Option<Decimal>,
    /// The reconstruction `move_delivered_qty − move_to_refund_qty` (the return-adjusted
    /// delivered quantity). `None` iff the `move_*` figures are.
    pub reconstructed_delivered_qty: Option<Decimal>,
}

/// The order-wide delivery view (`order_delivery_view`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SalesOrderDeliveryDto {
    pub order_id: Uuid,
    pub lines: Vec<SalesOrderDeliveryLineDto>,
}

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
    /// **Bounded** (council 2026-07-27) — the delivery twin of [`Self::mark_invoiced`]: it routes
    /// through a capacity-checked, `FOR UPDATE`-serialized allocation capped at each line's
    /// `quantity`, and **rejects** an over-delivery (`OverDelivered`). Without this, a racy/repeat
    /// `StockDelivered` or an inbound delivery for more than was ordered could push `delivered_qty`
    /// past `quantity`, while `recompute_order_status` (`delivered_qty >= quantity`) silently masks
    /// it as the delivered band — and `completed` could become true for stock that was never ordered.
    /// Serializing the *writer* (not just the upstream remainder) is what closes the race; the cap is
    /// what closes the over-delivery. Aggregate-by-item, fill in line order — correct even for
    /// duplicate-item orders.
    pub async fn mark_delivered(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        deliveries: &[(Uuid, Decimal)],
    ) -> Result<(), SellingError> {
        // RLS scope (ADR-0008): company on the parameter — the allocation tx binds it explicitly
        // (`bind_company_on`), and the status recompute runs inside the scope. The inbound handler for
        // inventory's `StockDelivered` passes the event's company; an event/job caller can no longer
        // forget to scope the `FOR UPDATE` reads inside `allocate_delivered`.
        company_scope::with_company_scope(Some(company_id), async move {
            let mut tx = self.db_pool.begin().await?;
            company_scope::bind_company_on(&mut tx, company_id).await?;
            for (item_id, qty) in deliveries {
                self.allocate_delivered(&mut tx, order_id, *item_id, *qty).await?;
            }
            tx.commit().await?;
            self.recompute_order_status(order_id).await?;
            Ok(())
        }).await
    }

    /// Fill `delivered_qty` up to `quantity` across an item's order lines (`FOR UPDATE`,
    /// fill-in-order); reject when the requested qty exceeds total remaining capacity
    /// (`quantity − delivered_qty`). Mirror of `allocate_billed` (council 2026-07-27).
    ///
    /// The lock-read and the writes MUST share the caller's `tx` — that is what serializes concurrent
    /// deliverers; splitting them across connections would re-open the over-delivery race.
    async fn allocate_delivered(&self, tx: &mut sqlx::PgConnection, order_id: Uuid, item_id: Uuid, mut qty: Decimal) -> Result<(), SellingError> {
        let lines = self.repos.order_items.lock_delivery_capacity(&mut *tx, order_id, item_id).await?;
        let total_cap: Decimal = lines.iter().map(|r| r.capacity).sum();
        if qty > total_cap {
            return Err(SellingError::OverDelivered);
        }
        for line in &lines {
            if qty <= Decimal::ZERO { break; }
            let cap = line.capacity;
            if cap <= Decimal::ZERO { continue; }
            let take = if qty < cap { qty } else { cap };
            self.repos.order_items.add_delivered_qty_on_line(&mut *tx, line.id, take).await?;
            qty -= take;
        }
        Ok(())
    }

    /// The move-backed delivery view: per live non-downpayment line, the ordered quantity, the
    /// stored watermark, and — when the stock engine has figures — the raw move figures plus the
    /// RETURN-ADJUSTED reconstruction `delivered − to_refund` (the sale_stock `qty_delivered`
    /// compute). Pure read: nothing is written; the raw reconstruction can exceed the ordered
    /// quantity (the physical moves over-delivered) and is shown as-is so the over-delivery is
    /// visible rather than masked.
    pub async fn order_delivery_view(
        &self,
        order_id: Uuid,
        stock: &dyn StockFulfillmentPort,
    ) -> Result<SalesOrderDeliveryDto, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — the header read rides the request-dedicated
        // connection (see `convert_quotation_to_order`); having read the order, its own company
        // scopes the line read and the port request below.
        let hdr = self.repos.orders.find_stock_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        let lines = company_scope::with_company_scope(
            Some(hdr.company_id),
            self.repos.order_items.list_stock_demand_lines(&self.db_pool, order_id),
        ).await?;
        let figures = stock
            .delivered_quantities(&DeliveredQtyRequest {
                company_id: hdr.company_id,
                order_id,
                lines: lines
                    .iter()
                    .map(|l| DeliveredQtyLineRef { line_id: l.id, item_id: l.item_id })
                    .collect(),
            })
            .await
            .map_err(|e| SellingError::FulfillmentRejected { code: e.code, message: e.message })?;
        let fig_of = |line_id: Uuid| figures.iter().find(|f| f.line_id == line_id);
        Ok(SalesOrderDeliveryDto {
            order_id,
            lines: lines
                .iter()
                .map(|l| {
                    let fig: Option<&MoveDeliveryFigures> = fig_of(l.id);
                    SalesOrderDeliveryLineDto {
                        line_id: l.id,
                        item_id: l.item_id,
                        quantity: l.quantity,
                        stored_delivered_qty: l.delivered_qty,
                        move_delivered_qty: fig.map(|f| f.delivered_qty),
                        move_returned_qty: fig.map(|f| f.returned_qty),
                        move_to_refund_qty: fig.map(|f| f.to_refund_qty),
                        reconstructed_delivered_qty: fig
                            .map(|f| f.delivered_qty - f.to_refund_qty),
                    }
                })
                .collect(),
        })
    }

    /// Reconcile the stored `delivered_qty` watermarks to the move-backed reconstruction (the
    /// sale_stock `qty_delivered` write path): per line with move figures, SET the watermark to
    /// `min(delivered − to_refund, quantity)` — clamped at the ordered quantity because the
    /// watermark invariant `delivered_qty <= quantity` is selling's own (an over-delivery stays
    /// visible in [`Self::order_delivery_view`]'s raw figures instead) — then recompute the
    /// order status from the watermarks.
    ///
    /// A REPLACE, not an add: a refund-shaped return legitimately LOWERS a watermark a previous
    /// delivery event had advanced (10 delivered, 3 returned to-refund → 7), which can move an
    /// order back from `to_bill` to `to_deliver_and_bill` — the status recompute follows the
    /// watermarks in either direction. Lines the stock engine reported NO figure for keep their
    /// stored watermark untouched (absence is never zero); with no figures at all the whole sync
    /// is a no-op. Concurrent `mark_delivered` allocations can interleave between the read and
    /// the write — the moves remain the truth a later sync reconciles to.
    pub async fn sync_delivered_from_moves(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        stock: &dyn StockFulfillmentPort,
    ) -> Result<(), SellingError> {
        let lines = company_scope::with_company_scope(
            Some(company_id),
            self.repos.order_items.list_stock_demand_lines(&self.db_pool, order_id),
        ).await?;
        if lines.is_empty() {
            return Ok(());
        }
        let figures = stock
            .delivered_quantities(&DeliveredQtyRequest {
                company_id,
                order_id,
                lines: lines
                    .iter()
                    .map(|l| DeliveredQtyLineRef { line_id: l.id, item_id: l.item_id })
                    .collect(),
            })
            .await
            .map_err(|e| SellingError::FulfillmentRejected { code: e.code, message: e.message })?;
        // Only lines the engine reported figures for are written; clamp each at its ordered
        // quantity (the watermark invariant — the raw figure stays visible in the view).
        let updates: Vec<(Uuid, Decimal)> = lines
            .iter()
            .filter_map(|l| {
                figures
                    .iter()
                    .find(|f| f.line_id == l.id)
                    .map(|f| {
                        let net = f.delivered_qty - f.to_refund_qty;
                        (l.id, if net > l.quantity { l.quantity } else { net })
                    })
            })
            .collect();
        if updates.is_empty() {
            return Ok(()); // no move-backed figures: keep every stored watermark
        }
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, company_id).await?;
        self.repos.order_items.set_delivered_quantities(&mut tx, order_id, &updates).await?;
        tx.commit().await?;
        self.recompute_order_status(order_id).await?;
        Ok(())
    }
}
