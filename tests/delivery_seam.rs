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

use backbone_selling::application::service::selling_gl::{
    AccountingPostEnvelope as SellEnv, GlPostAck as SellAck, GlPostRejected as SellRej, GlPostSink as SellSink,
};
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesInvoice, NewSalesOrder, SellingWriteService,
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

// One ACL adapter maps EITHER module's envelope into accounting's PostingRequest. Both selling and
// inventory post into the SAME real ledger.
struct GlAdapter { svc: PostingService }
fn to_req(company: Uuid, source_type: &str, source_id: Uuid, date: chrono::NaiveDate,
          lines: Vec<PostingLine>, reference: Option<String>) -> PostingRequest {
    let mut r = PostingRequest::original(company, source_type, source_id, date);
    r.source_reference = reference;
    r.lines = lines;
    r
}
#[async_trait::async_trait]
impl SellSink for GlAdapter {
    async fn post(&self, e: &SellEnv) -> Result<SellAck, SellRej> {
        let lines = e.lines.iter().map(|l| PostingLine {
            account_id: l.account_id, debit: l.debit, credit: l.credit,
            party_type: l.party_type.clone(), party_id: l.party_id,
            cost_center_id: None, project_id: None, department_id: None, description: l.description.clone(),
        }).collect();
        match self.svc.post(to_req(e.company_id, &e.source_type, e.source_id, e.posting_date, lines, e.source_reference.clone()), None).await {
            Ok(r) => Ok(SellAck { post_id: r.post_id, journal_id: r.journal_id, idempotent_reuse: r.idempotent_reuse }),
            Err(x) => Err(SellRej { code: x.code().to_string(), message: x.to_string() }),
        }
    }
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
    let gl = GlAdapter { svc: PostingService::new(pool.clone()) };

    // 1) inventory receives 10 @ 100 into a warehouse.
    let wh = inventory.create_warehouse(NewWarehouse { company_id: company, code: uq("WH"), name: "Main".into(), warehouse_type: None, parent_warehouse_id: None, is_group: false }).await.unwrap();
    let rid = inventory.create_purchase_receipt(NewReceipt {
        receipt_number: uq("PR"), company_id: company, branch_id: None, supplier_id: Uuid::new_v4(),
        source_po_id: None, warehouse_id: wh, posting_date: day(),
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
        posting_date: day(), cogs_account_id: coa["5100"], inventory_account_id: coa["1300"],
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
    selling.mark_delivered(oid, &[(item, d("10"))]).await.unwrap();
    assert_eq!(order_status(&pool, oid).await, "to_bill", "delivered, still awaiting billing");

    // 6) selling bills FROM the order (links lines so billed_qty advances) + posts → revenue post.
    let inv = selling.create_invoice_from_order(oid, uq("INV"), day(), coa["1200"], coa["4000"], Some(coa["2200"])).await.unwrap();
    let po = selling.post_sales_invoice(inv, &gl).await.unwrap();
    // Revenue journal: Dr A/R 1,665,000 · Cr Revenue 1,500,000 · Cr PPN 165,000.
    assert_eq!(journal_totals(&pool, po.journal_id).await, (d("1665000"), d("1665000")));

    // 7) order is billed AND delivered → completed. Both journals exist for the company.
    assert_eq!(order_status(&pool, oid).await, "completed");
    let n_journals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounting.journals WHERE company_id=$1").bind(company).fetch_one(&pool).await.unwrap();
    assert_eq!(n_journals, 3, "goods-receipt + COGS + revenue journals");
    // inventory Bin drained to 0 (residual flush).
    let (bin_qty, bin_val): (Decimal, Decimal) = sqlx::query_as("SELECT actual_qty, stock_value FROM inventory.bins WHERE company_id=$1 AND item_id=$2 AND warehouse_id=$3").bind(company).bind(item).bind(wh).fetch_one(&pool).await.unwrap();
    assert_eq!(bin_qty, d("0.0000"));
    assert_eq!(bin_val, d("0.00"));
}

async fn order_status(pool: &PgPool, oid: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1").bind(oid).fetch_one(pool).await.unwrap()
}
async fn journal_totals(pool: &PgPool, jid: Uuid) -> (Decimal, Decimal) {
    let r = sqlx::query("SELECT total_debit, total_credit FROM accounting.journals WHERE id=$1").bind(jid).fetch_one(pool).await.unwrap();
    (r.get("total_debit"), r.get("total_credit"))
}
