//! The MARQUEE cross-module seam, end-to-end across THREE modules: **selling → inventory →
//! accounting → selling**, with zero normal Cargo edges (inventory + accounting are dev-deps only).
//!
//! Flow (order-to-cash + fulfillment):
//!   1. inventory receives stock (so there is something to ship)
//!   2. selling: create + confirm a Sales Order  → `to_deliver_and_bill`
//!   3. selling emits a `DeliveryRequestEnvelope`; an ACL adapter maps it into inventory's
//!      `DeliveryRequested` (adding warehouse + GL accounts inventory owns) → a draft Delivery Note
//!   4. inventory submits the Delivery Note → **COGS post** into the REAL accounting ledger + a
//!      `StockDelivered` event
//!   5. an ACL routes `StockDelivered` → selling `mark_delivered` → `delivered_qty` advances
//!   6. selling bills + posts the invoice → **revenue post** into the same ledger
//!   7. order reaches `completed` (billed AND delivered); accounting holds BOTH journals.
//!
//! Every cross-module hop is a serialized envelope mapped by an in-test ACL — no module imports
//! another. All three schemas (`selling.*`, `inventory.*`, `accounting.*`) live in one DB.
//! Requires DATABASE_URL (:5433/backbone_selling with all three schemas migrated).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesOrder, SellingError, SellingWriteService,
};

use backbone_inventory::application::service::inventory_gl::{
    AccountingPostEnvelope as InvEnv, GlPostAck as InvAck, GlPostRejected as InvRej, GlPostSink as InvSink,
};
use backbone_inventory::application::service::inventory_events::{InventoryEvent, InventoryEventSink};
use backbone_inventory::application::service::inventory_intake::{DeliveryIntake, DeliveryRequestLine as InvReqLine, DeliveryRequested};
use backbone_inventory::application::service::inventory_write_service::{
    InventoryWriteService, NewReceipt, NewWarehouse, ReceiptLine,
};

use backbone_accounting::application::service::posting_service::{PostingLine, PostingRequest, PostingService};
use backbone_accounting::infrastructure::persistence::SqlxPostingRepository;

// The ACL adapter maps inventory's envelope into accounting's PostingRequest. (Selling no longer
// posts — it exited the invoice business; ADR-006 — so GlAdapter only impls InvSink now.)
struct GlAdapter { svc: PostingService }
fn to_req(company: Uuid, source_type: &str, source_id: Uuid, date: chrono::NaiveDate,
          lines: Vec<PostingLine>, reference: Option<String>) -> PostingRequest {
    let mut r = PostingRequest::original(company, source_type, source_id, date);
    r.source_reference = reference;
    r.lines = lines;
    r
}
#[async_trait::async_trait]
impl InvSink for GlAdapter {
    async fn post(&self, e: &InvEnv) -> Result<InvAck, InvRej> {
        let lines = e.lines.iter().map(|l| PostingLine {
            account_id: l.account_id, debit: l.debit, credit: l.credit,
            party_type: l.party_type.clone(), party_id: l.party_id,
            cost_center_id: None, project_id: None, department_id: None, description: l.description.clone(),
        }).collect();
        match self.svc.post(to_req(e.company_id, &e.source_type, e.source_id, e.posting_date, lines, e.source_reference.clone()), None).await {
            Ok(r) => Ok(InvAck { post_id: r.post_id, journal_id: r.journal_id, idempotent_reuse: r.idempotent_reuse }),
            Err(x) => Err(InvRej { code: x.code().to_string(), message: x.to_string() }),
        }
    }
}

// Recording sink for inventory domain events (captures StockDelivered).
#[derive(Default, Clone)]
struct RecordingInvSink { events: Arc<Mutex<Vec<InventoryEvent>>> }
impl InventoryEventSink for RecordingInvSink {
    fn publish(&self, e: InventoryEvent) { self.events.lock().unwrap().push(e); }
}

fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }
fn day() -> chrono::NaiveDate { chrono::NaiveDate::from_ymd_opt(2026, 7, 4).unwrap() }
fn uq(p: &str) -> String { format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8]) }
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}
async fn seed_coa(pool: &PgPool) -> (Uuid, HashMap<&'static str, Uuid>) {
    let company = Uuid::new_v4();
    let coa: &[(&str, &str, &str, &str, &str, bool, bool)] = &[
        ("1200", "Piutang", "asset", "accounts_receivable", "debit", false, true),
        ("1300", "Persediaan", "asset", "inventory", "debit", false, true),
        ("2150", "GR/IR", "liability", "current_liability", "credit", false, true),
        ("2200", "PPN Keluaran", "liability", "tax", "credit", false, true),
        ("4000", "Pendapatan", "revenue", "operating_revenue", "credit", false, true),
        ("5100", "HPP", "cogs", "direct_cost", "debit", false, true),
    ];
    let mut m = HashMap::new();
    for (code, name, at, st, nb, h, det) in coa {
        let id = Uuid::new_v4();
        sqlx::query(r#"INSERT INTO accounting.accounts (id, company_id, account_number, account_code, name, account_type, account_subtype, normal_balance, is_header, is_detail, status)
            VALUES ($1,$2,$3,$4,$5,$6::account_type,$7::account_subtype,$8::normal_balance,$9,$10,'active'::account_status)"#)
            .bind(id).bind(company).bind(code).bind(code).bind(name).bind(at).bind(st).bind(nb).bind(h).bind(det)
            .execute(pool).await.expect("seed acct");
        m.insert(*code, id);
    }
    (company, m)
}

/// DSEAM-1: the full order-to-cash + fulfillment round-trip across selling, inventory, and the real
/// accounting ledger — proving both the revenue and COGS journals land and the order completes.
#[tokio::test]
async fn order_to_cash_and_fulfillment_across_three_modules() {
    let pool = pool().await;
    let (company, coa) = seed_coa(&pool).await;
    let customer = Uuid::new_v4();
    let item = Uuid::new_v4();

    let selling = SellingWriteService::new(pool.clone());
    let recorder = RecordingInvSink::default();
    let inventory = InventoryWriteService::with_sink(pool.clone(), Arc::new(recorder.clone()));
    let intake = DeliveryIntake::new(pool.clone());
    let gl = GlAdapter { svc: PostingService::new(Arc::new(SqlxPostingRepository::new(pool.clone()))) };

    // 1) inventory receives 10 @ 100 into a warehouse.
    let wh = inventory.create_warehouse(NewWarehouse { company_id: company, code: uq("WH"), name: "Main".into(), warehouse_type: None, parent_warehouse_id: None, is_group: false }).await.unwrap();
    let rid = inventory.create_purchase_receipt(NewReceipt {
        receipt_number: uq("PR"), company_id: company, branch_id: None, supplier_id: Uuid::new_v4(),
        source_po_id: None, warehouse_id: wh, posting_date: day(), currency: "IDR".into(),
        inventory_account_id: coa["1300"], grir_account_id: coa["2150"],
        lines: vec![ReceiptLine { item_id: item, quantity: d("10"), rate: d("100") }],
    }).await.unwrap();
    inventory.submit_purchase_receipt(rid, &gl).await.unwrap();

    // 2) selling: create + confirm a Sales Order for 10 of that item.
    let oid = selling.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, company_id: company, branch_id: None,
        customer_id: customer, order_date: day(), delivery_date: None, currency: None,
        tax_rate: d("11"), notes: None,
        lines: vec![NewLine { item_id: item, revenue_account_id: None, description: None,
            quantity: d("10"), unit_price: d("150000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();
    selling.confirm_sales_order(oid, company).await.unwrap();
    assert_eq!(order_status(&pool, oid).await, "to_deliver_and_bill");

    // 3) selling emits a delivery request; ACL maps it into inventory's DeliveryRequested.
    let req = selling.build_delivery_request(oid).await.unwrap();
    assert_eq!(req.lines.len(), 1);
    let dn = intake.on_delivery_requested(DeliveryRequested {
        delivery_number: uq("DN"), company_id: req.company_id, branch_id: None,
        customer_id: req.customer_id, source_so_id: Some(req.order_id), warehouse_id: wh,
        posting_date: day(), currency: "IDR".into(), cogs_account_id: coa["5100"], inventory_account_id: coa["1300"],
        lines: req.lines.iter().map(|l| InvReqLine { item_id: l.item_id, quantity: l.quantity }).collect(),
    }).await.unwrap();

    // 4) inventory submits the delivery → COGS post into the REAL ledger + StockDelivered.
    let out = inventory.submit_delivery_note(dn, &gl).await.unwrap();
    assert!(out.posted);
    // COGS journal: Dr COGS 1000 (10 @ moving-avg 100) · Cr Inventory 1000.
    assert_eq!(journal_totals(&pool, out.journal_id.unwrap()).await, (d("1000"), d("1000")));

    // 5) ACL: the StockDelivered event (source_so_id = our order) drives selling.mark_delivered.
    let evts = recorder.events.lock().unwrap().clone();
    let delivered_so = evts.iter().find_map(|e| match e {
        InventoryEvent::StockDelivered(s) if s.source_so_id == Some(oid) => Some(s.clone()), _ => None,
    }).expect("StockDelivered for our order");
    assert_eq!(delivered_so.total_cogs, d("1000.00"));
    // We know the delivered lines from the request we routed (the composition's correspondence).
    selling.mark_delivered(oid, delivered_so.company_id, &[(item, d("10"))]).await.unwrap();
    assert_eq!(order_status(&pool, oid).await, "to_bill", "delivered, still awaiting billing");

    // (The revenue leg — billing posts the invoice, the order reaches `completed`, and the Bin
    // residual flushes — is owned by backbone-billing and proven in tests/invoice_seam.rs +
    // backbone-billing/tests/ar_seam.rs. This test stops at `to_bill` because selling exited the
    // invoice business; ADR-006.)
}

async fn order_status(pool: &PgPool, oid: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1").bind(oid).fetch_one(pool).await.unwrap()
}
async fn journal_totals(pool: &PgPool, jid: Uuid) -> (Decimal, Decimal) {
    let r = sqlx::query("SELECT total_debit, total_credit FROM accounting.journals WHERE id=$1").bind(jid).fetch_one(pool).await.unwrap();
    (r.get("total_debit"), r.get("total_credit"))
}

// ── helpers for the delivered-qty capacity tests (mirror of the invoice seam's billing-capacity
//    helpers). These exercise selling alone — no inventory/accounting — so they need only the
//    selling schema + RLS at DATABASE_URL. ─────────────────────────────────────────────────────
fn dline(item: Uuid, qty: &str) -> NewLine {
    NewLine {
        item_id: item, revenue_account_id: None, description: None,
        quantity: d(qty), unit_price: d("150000"), line_discount: Decimal::ZERO,
    }
}
async fn confirmed_deliverable_order(
    selling: &SellingWriteService, company: Uuid, lines: Vec<NewLine>,
) -> Uuid {
    let order = selling.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, company_id: company, branch_id: None,
        customer_id: Uuid::new_v4(), order_date: day(), delivery_date: None, currency: None,
        tax_rate: Decimal::ZERO, notes: None, lines,
    }).await.unwrap();
    selling.confirm_sales_order(order, company).await.unwrap();
    order
}
async fn delivered_total(pool: &PgPool, order: Uuid) -> Decimal {
    sqlx::query_scalar("SELECT COALESCE(SUM(delivered_qty),0) FROM selling.sales_order_items WHERE order_id=$1")
        .bind(order).fetch_one(pool).await.unwrap()
}

// DSEAM-2 (council 2026-07-27): `mark_delivered` is BOUNDED — you cannot deliver past the ordered
// quantity. A repeat/racy `StockDelivered`, or an inbound delivery for more than was ordered, is
// refused at the writer, so `delivered_qty` never exceeds `quantity` and `recompute_order_status`
// (`delivered_qty >= quantity`) cannot silently mask an over-delivery as a completed band. Without
// the cap a single `mark_delivered` of 11 on a 10 runs `delivered_qty` to 11 and the order reaches
// the delivered band with no error.
#[tokio::test]
async fn over_delivery_is_refused() {
    let pool = pool().await;
    let selling = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_deliverable_order(&selling, company, vec![dline(item, "10")]).await;
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");

    // 11 > ordered 10 → refused; the watermark stays 0 and the order stays to_deliver_and_bill.
    let e = selling.mark_delivered(order, company, &[(item, d("11"))]).await.unwrap_err();
    assert!(matches!(e, SellingError::OverDelivered));
    assert_eq!(
        delivered_total(&pool, order).await, d("0.0000"),
        "a rejected mark_delivered leaves the watermark untouched (tx rolled back)",
    );
    assert_eq!(order_status(&pool, order).await, "to_deliver_and_bill");
}

// DSEAM-3 (council 2026-07-27): the aggregate-by-item delivery allocation is correct for
// duplicate-item orders — two lines of item X (6 + 4) have total delivery capacity 10; delivering
// 12 is refused, delivering 10 fills both lines to their caps. (Mirror of the invoice seam's
// `duplicate_item_lines_allocate_by_capacity`.)
#[tokio::test]
async fn duplicate_item_lines_allocate_delivery_by_capacity() {
    let pool = pool().await;
    let selling = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_deliverable_order(&selling, company, vec![dline(item, "6"), dline(item, "4")]).await;

    // 12 > total capacity 10 → refused, nothing advances.
    assert!(matches!(
        selling.mark_delivered(order, company, &[(item, d("12"))]).await.unwrap_err(),
        SellingError::OverDelivered,
    ));
    assert_eq!(delivered_total(&pool, order).await, d("0.0000"));

    // 10 fills both lines to their caps (6 then 4, fill-in-id order).
    selling.mark_delivered(order, company, &[(item, d("10"))]).await.unwrap();
    let caps: Vec<Decimal> = sqlx::query_scalar(
        "SELECT delivered_qty FROM selling.sales_order_items WHERE order_id=$1 ORDER BY quantity DESC",
    ).bind(order).fetch_all(&pool).await.unwrap();
    assert_eq!(caps, vec![d("6.0000"), d("4.0000")]);
}
