//! The sale_stock confirm engine — selling's stock-fulfillment port (hand-authored, user-owned).
//!
//! Proves the three port behaviors against a SCRIPTED fake (the composition's stock-engine
//! adapter stand-in — no inventory crate is involved; that is the point of the port):
//!
//!   confirm launches stock rules per STORABLE line — the request models the
//!   procurement-group intent (order identity + one entry per non-downpayment live line); the
//!   port launches only stock-tracked items (a service line is a skip, not an error); a port
//!   Err REFUSES the whole confirm fail-closed (order stays draft, no cost stamp, retryable);
//!
//!   qty_delivered RECONSTRUCTS from moves, return-adjusted — the view derives
//!   `delivered − to_refund` per line (an exchanged return does not reduce it); the sync
//!   REPLACES the stored watermark with that reconstruction (a refund-shaped return can lower
//!   it), clamped at the ordered quantity (the watermark invariant; the raw figure stays
//!   visible in the view); a line with NO move figure keeps its watermark (absence ≠ zero);
//!
//!   cancellation LOGS DECREASE-QUANTITY ACTIVITIES upstream instead of silently un-reserving
//!   — the log is requested only after the guarded flip committed; a log failure does
//!   not undo the cancellation (the event still fires) and is retried through the retry verb;
//!   the billed-lines refusal happens with NO port call at all.
//!
//! Requires DATABASE_URL pointing at a scratch DB with the selling migrations applied.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::selling_events::{SellingEvent, SellingEventSink};
use backbone_selling::application::service::selling_stock_fulfillment::{
    DecreaseQuantityRequest, DeliveredQtyRequest, MoveDeliveryFigures, NoStockFulfillmentPort,
    StockFulfillmentError, StockFulfillmentPort, StockRuleOutcome, StockRuleRequest,
};
use backbone_selling::application::service::selling_unit_cost::NoUnitCostPort;
use backbone_selling::application::service::selling_service_catalog::NoServiceCatalog;
use backbone_selling::application::service::selling_service_delivery::NoServiceDelivery;
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesOrder, SellingError, SellingWriteService,
};

// ── the scripted fake (the composition's stock-engine adapter stand-in) ───────

/// A scriptable `StockFulfillmentPort`: records every request, plays back canned figures and
/// per-item storable-ness, and can be armed to refuse either write side.
#[derive(Default, Clone)]
struct FakeStockPort {
    launches: Arc<Mutex<Vec<StockRuleRequest>>>,
    figures: Arc<Mutex<Vec<MoveDeliveryFigures>>>,
    logs: Arc<Mutex<Vec<DecreaseQuantityRequest>>>,
    /// Items the engine treats as stock-tracked (empty = nothing is).
    storable: Arc<Mutex<HashSet<Uuid>>>,
    launch_err: Arc<Mutex<Option<StockFulfillmentError>>>,
    log_err: Arc<Mutex<Option<StockFulfillmentError>>>,
}

impl FakeStockPort {
    fn err(code: &str, message: &str) -> StockFulfillmentError {
        StockFulfillmentError { code: code.into(), message: message.into() }
    }
}

#[async_trait]
impl StockFulfillmentPort for FakeStockPort {
    async fn launch_stock_rules(
        &self,
        req: &StockRuleRequest,
    ) -> Result<Vec<StockRuleOutcome>, StockFulfillmentError> {
        self.launches.lock().unwrap().push(req.clone());
        if let Some(e) = self.launch_err.lock().unwrap().take() {
            return Err(e);
        }
        Ok(req
            .lines
            .iter()
            .map(|l| {
                let launched = self.storable.lock().unwrap().contains(&l.item_id);
                StockRuleOutcome {
                    line_id: l.line_id,
                    launched,
                    move_id: launched.then(Uuid::new_v4),
                    picking_id: launched.then(Uuid::new_v4),
                    procure_method: launched.then(|| "make_to_stock".to_string()),
                }
            })
            .collect())
    }

    async fn delivered_quantities(
        &self,
        _req: &DeliveredQtyRequest,
    ) -> Result<Vec<MoveDeliveryFigures>, StockFulfillmentError> {
        Ok(self.figures.lock().unwrap().clone())
    }

    async fn log_decrease_quantity(
        &self,
        req: &DecreaseQuantityRequest,
    ) -> Result<(), StockFulfillmentError> {
        self.logs.lock().unwrap().push(req.clone());
        if let Some(e) = self.log_err.lock().unwrap().take() {
            return Err(e);
        }
        Ok(())
    }
}

/// Records the domain events the service publishes (the cancel-fires-anyway assertion).
#[derive(Default, Clone)]
struct RecordingSink { events: Arc<Mutex<Vec<SellingEvent>>> }
impl SellingEventSink for RecordingSink {
    fn publish(&self, e: SellingEvent) { self.events.lock().unwrap().push(e); }
}

// ── fixtures ──────────────────────────────────────────────────────────────────

fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }
fn uq(p: &str) -> String { format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8]) }
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}
fn line(item: Uuid, qty: &str) -> NewLine {
    NewLine {
        invoice_policy: None, is_downpayment: None, revenue_account_id: None, description: None,
        item_id: item, quantity: d(qty), unit_price: d("150000"), line_discount: Decimal::ZERO,
    }
}
fn downpayment_line(item: Uuid, qty: &str) -> NewLine {
    NewLine {
        invoice_policy: None, is_downpayment: Some(true), revenue_account_id: None,
        description: None, item_id: item, quantity: d(qty), unit_price: d("50000"),
        line_discount: Decimal::ZERO,
    }
}
async fn draft_order(w: &SellingWriteService, company: Uuid, lines: Vec<NewLine>) -> Uuid {
    let n = uq("SO");
    let order = w
        .create_sales_order(NewSalesOrder {
            order_number: n, quotation_id: None, delivery_carrier_id: None, company_id: company,
            branch_id: None, customer_id: Uuid::new_v4(),
            order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 27).unwrap(),
            delivery_date: None, currency: None, tax_rate: Decimal::ZERO, notes: None, lines,
        })
        .await
        .unwrap();
    order
}
async fn order_status(pool: &PgPool, oid: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(oid)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn line_watermark(pool: &PgPool, order: Uuid, item: Uuid) -> Decimal {
    sqlx::query_scalar(
        "SELECT delivered_qty FROM selling.sales_order_items WHERE order_id=$1 AND item_id=$2 ORDER BY id",
    )
    .bind(order)
    .bind(item)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── (a) confirm launches stock rules per storable line ────────────────────────

// Launch request shape: it models the procurement-group intent — order identity + ONE entry
// per live NON-DOWNPAYMENT line with its full ordered quantity; the port launches only the
// stock-tracked items (the service line is a skip, not an error); the confirm succeeds.
#[tokio::test]
async fn confirm_launches_rules_per_storable_line_only() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, storable, service) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    *port.storable.lock().unwrap() = HashSet::from([storable]);

    let order = draft_order(
        &w,
        company,
        vec![line(storable, "10"), line(service, "2"), downpayment_line(service, "1")],
    )
    .await;
    let order_number = sqlx::query_scalar::<_, String>(
        "SELECT order_number FROM selling.sales_orders WHERE id=$1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();

    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");

    // Exactly one launch, carrying the order's identity + the TWO non-downpayment lines.
    let launches = port.launches.lock().unwrap();
    assert_eq!(launches.len(), 1, "one procurement-group launch per confirm");
    let req = &launches[0];
    assert_eq!(req.order_id, order);
    assert_eq!(req.company_id, company);
    assert_eq!(req.order_number, order_number);
    assert_eq!(req.lines.len(), 2, "the downpayment line never drives stock work");
    let by_item = |it: Uuid| req.lines.iter().find(|l| l.item_id == it).expect("line present");
    assert_eq!(by_item(storable).quantity, d("10"));
    assert_eq!(by_item(service).quantity, d("2"));
}

// Refusal semantics: a refused launch refuses the WHOLE confirm fail-closed — the order stays draft, no
// unit-cost stamp was written, the port's code rides verbatim, and the refusal is not sticky:
// a retried confirm with a healthy engine succeeds.
#[tokio::test]
async fn refused_launch_leaves_the_order_draft_and_is_retryable() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    *port.storable.lock().unwrap() = HashSet::from([item]);
    *port.launch_err.lock().unwrap() = Some(FakeStockPort::err("no_rule_for_demand", "no active pull rule covers the demand"));

    let order = draft_order(&w, company, vec![line(item, "5")]).await;
    match w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap_err() {
        SellingError::FulfillmentRejected { code, .. } => assert_eq!(code, "no_rule_for_demand"),
        other => panic!("expected FulfillmentRejected, got {other:?}"),
    }
    assert_eq!(order_status(&pool, order).await, "draft", "the order stays draft");
    let stamped: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM selling.sales_order_items WHERE order_id=$1 AND unit_cost IS NOT NULL",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stamped, 0, "a refused launch wrote no cost stamp (stamp rides the confirm tx)");

    // Not sticky: the armed error is consumed; the retry launches and confirms.
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");
    assert_eq!(port.launches.lock().unwrap().len(), 2);
}

// Downpayment-only orders never ask the stock engine anything — there is no line
// that could ever ship.
#[tokio::test]
async fn downpayment_only_order_never_launches() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    let order = draft_order(&w, company, vec![downpayment_line(item, "1")]).await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    assert!(port.launches.lock().unwrap().is_empty());
}

// ── (b) qty_delivered reconstructs from moves, return-adjusted ────────────────

// Reconstruction rule: `delivered − to_refund` — an EXCHANGED return (returned but not
// to-refund) does not reduce the delivered commitment; the sync REPLACES the stored watermark
// with the reconstruction and can LOWER it when a refund-shaped return lands, with the order
// status following the watermarks back down.
#[tokio::test]
async fn delivered_reconstruction_subtracts_to_refund_returns_only() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    let order = draft_order(&w, company, vec![line(item, "10")]).await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();

    // 10 delivered gross, 3 returned of which 1 to-refund → net 9 (the 2 exchanged stay delivered).
    *port.figures.lock().unwrap() = vec![MoveDeliveryFigures {
        line_id: line_id_of(&pool, order, item).await,
        delivered_qty: d("10"),
        returned_qty: d("3"),
        to_refund_qty: d("1"),
    }];
    let view = w.order_delivery_view(order, &port).await.unwrap();
    assert_eq!(view.lines.len(), 1);
    let l = &view.lines[0];
    assert_eq!(l.move_delivered_qty, Some(d("10")));
    assert_eq!(l.move_returned_qty, Some(d("3")));
    assert_eq!(l.move_to_refund_qty, Some(d("1")));
    assert_eq!(l.reconstructed_delivered_qty, Some(d("9")), "net of the to-refund return only");

    w.sync_delivered_from_moves(order, company, &port).await.unwrap();
    assert_eq!(line_watermark(&pool, order, item).await, d("9"));
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill", "1 of 10 still owed");

    // The customer refunds the rest of the returns: net drops to 7 — the watermark FOLLOWS IT
    // DOWN (a replace, not an add) and the order stays in the awaiting-delivery band.
    port.figures.lock().unwrap()[0].to_refund_qty = d("3");
    w.sync_delivered_from_moves(order, company, &port).await.unwrap();
    assert_eq!(line_watermark(&pool, order, item).await, d("7"), "a refund-shaped return lowers the watermark");
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");

    // Fully delivered and un-returned: the watermark reaches the ordered quantity.
    port.figures.lock().unwrap()[0].to_refund_qty = d("0");
    w.sync_delivered_from_moves(order, company, &port).await.unwrap();
    assert_eq!(order_status(&pool, order).await, "to_bill", "delivered band once the reconstruction covers the order");
}

// Watermark clamping: the stored watermark is CLAMPED at the ordered quantity even when the physical moves
// over-delivered (the raw figure stays visible in the view); a line the engine reported NO
// figure for keeps its stored watermark (absence is never zero); with no figures at all the
// sync is a total no-op — the No-port composition can never erase a watermark.
#[tokio::test]
async fn reconstruction_clamps_at_ordered_qty_and_absence_is_not_zero() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, storable, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    let order = draft_order(&w, company, vec![line(storable, "10"), line(other, "4")]).await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();

    // The other line legitimately delivered 4 through the inbound event path.
    w.mark_delivered(order, company, &[(other, d("4"))]).await.unwrap();

    // The engine over-delivered storable (12 done moves on a 10-line) and knows nothing of `other`.
    *port.figures.lock().unwrap() = vec![MoveDeliveryFigures {
        line_id: line_id_of(&pool, order, storable).await,
        delivered_qty: d("12"),
        returned_qty: d("0"),
        to_refund_qty: d("0"),
    }];
    let view = w.order_delivery_view(order, &port).await.unwrap();
    let st = view.lines.iter().find(|l| l.item_id == storable).unwrap();
    let ot = view.lines.iter().find(|l| l.item_id == other).unwrap();
    assert_eq!(st.reconstructed_delivered_qty, Some(d("12")), "the view shows the RAW over-delivery");
    assert_eq!(ot.reconstructed_delivered_qty, None, "no figure for the line = absence, not zero");

    w.sync_delivered_from_moves(order, company, &port).await.unwrap();
    assert_eq!(line_watermark(&pool, order, storable).await, d("10"), "clamped at the ordered quantity");
    assert_eq!(line_watermark(&pool, order, other).await, d("4"), "absent lines keep their watermark");
    assert_eq!(order_status(&pool, order).await, "to_bill", "both lines at their delivered bands");

    // No figures at all: a total no-op (this is exactly what the No-port adapter returns).
    port.figures.lock().unwrap().clear();
    w.sync_delivered_from_moves(order, company, &NoStockFulfillmentPort).await.unwrap();
    assert_eq!(line_watermark(&pool, order, storable).await, d("10"));
    assert_eq!(line_watermark(&pool, order, other).await, d("4"));
}

async fn line_id_of(pool: &PgPool, order: Uuid, item: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM selling.sales_order_items WHERE order_id=$1 AND item_id=$2 ORDER BY id")
        .bind(order)
        .bind(item)
        .fetch_one(pool)
        .await
        .unwrap()
}

// ── (c) cancellation logs decrease-quantity activities upstream ──────────────

// Cancellation logging: a successful cancel asks the engine to LOG decrease-quantity activities for every
// non-downpayment line, carrying ordered vs shipped so an operator knows exactly how much
// confirmed demand went away. Selling itself never un-reserves anything — it holds no
// reservation to release; the log is the only upstream channel.
#[tokio::test]
async fn cancel_logs_decrease_quantity_upstream_per_line() {
    let pool = pool().await;
    let sink = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(sink.clone()));
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    // The downpayment line carries a throwaway item: `mark_delivered` allocates an inbound
    // delivery across ALL of an item's lines in line order, so sharing the item would make how
    // much lands on the demand line depend on the two lines' random id ordering.
    let order = draft_order(
        &w,
        company,
        vec![line(item, "10"), downpayment_line(Uuid::new_v4(), "1")],
    )
    .await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    w.mark_delivered(order, company, &[(item, d("4"))]).await.unwrap();

    w.cancel_sales_order(order, company, &port).await.unwrap();
    assert_eq!(order_status(&pool, order).await, "cancelled");

    let logs = port.logs.lock().unwrap();
    assert_eq!(logs.len(), 1, "one decrease-quantity log per cancel");
    let req = &logs[0];
    assert_eq!(req.order_id, order);
    assert_eq!(req.lines.len(), 1, "the downpayment line is not stock work");
    assert_eq!(req.lines[0].item_id, item);
    assert_eq!(req.lines[0].ordered_qty, d("10"));
    assert_eq!(req.lines[0].delivered_qty, d("4"), "ordered vs SHIPPED — what the operator must reconcile");
    assert!(sink.events.lock().unwrap().iter().any(|e| matches!(e, SellingEvent::SalesOrderCancelled(c) if c.order_id == order)));
}

// Log-failure semantics: a FAILED log does not undo the cancellation — the flip already committed, the
// SalesOrderCancelled event still fires (consumers must see the commitment's end), the method
// returns DecreaseActivityFailed loudly, and the retry verb completes the log once the engine
// is healthy. The retry refuses an order that is not cancelled.
#[tokio::test]
async fn failed_decrease_log_keeps_the_cancel_and_is_retriable() {
    let pool = pool().await;
    let sink = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(sink.clone()));
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    let order = draft_order(&w, company, vec![line(item, "6")]).await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();

    *port.log_err.lock().unwrap() = Some(FakeStockPort::err("engine_unavailable", "stock engine unreachable"));
    match w.cancel_sales_order(order, company, &port).await.unwrap_err() {
        SellingError::DecreaseActivityFailed { code, .. } => assert_eq!(code, "engine_unavailable"),
        other => panic!("expected DecreaseActivityFailed, got {other:?}"),
    }
    assert_eq!(order_status(&pool, order).await, "cancelled", "the cancellation itself committed");
    assert!(
        sink.events.lock().unwrap().iter().any(|e| matches!(e, SellingEvent::SalesOrderCancelled(c) if c.order_id == order)),
        "the cancel event fires even when the upstream log failed"
    );
    assert_eq!(port.logs.lock().unwrap().len(), 1, "the failed attempt was recorded, not swallowed");

    // The retry completes the log with the same per-line figures.
    w.retry_decrease_activities(order, company, &port).await.unwrap();
    assert_eq!(port.logs.lock().unwrap().len(), 2);
    assert_eq!(port.logs.lock().unwrap()[1].lines[0].ordered_qty, d("6"));

    // A retry against an order that is NOT cancelled is a loud refusal, not a quiet no-op.
    let live = draft_order(&w, company, vec![line(item, "2")]).await;
    assert!(matches!(
        w.retry_decrease_activities(live, company, &port).await.unwrap_err(),
        SellingError::InvalidTransition { .. }
    ));
}

// Billed-lines refusal: it happens with NO port
// call — a refused cancel has nothing to tell the stock side.
#[tokio::test]
async fn billed_refusal_makes_no_port_call() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let port = FakeStockPort::default();
    let order = draft_order(&w, company, vec![line(item, "3")]).await;
    w.confirm_sales_order(order, company, &NoUnitCostPort, &port, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    sqlx::query("UPDATE selling.sales_order_items SET billed_qty=3 WHERE order_id=$1")
        .bind(order)
        .execute(&pool)
        .await
        .unwrap();

    assert!(matches!(
        w.cancel_sales_order(order, company, &port).await.unwrap_err(),
        SellingError::OrderBilled
    ));
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");
    assert!(port.logs.lock().unwrap().is_empty(), "no decrease-quantity log for a refused cancel");
}
