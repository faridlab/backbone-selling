//! The order-to-cash INVOICE seam, end-to-end across THREE modules: **selling → billing →
//! accounting → selling** — retiring selling's *simulated* invoice leg with a REAL billing Sales
//! Invoice. Zero normal Cargo edges (billing + accounting are dev-deps only).
//!
//! Flow: selling confirms a Sales Order → emits an `InvoiceRequestEnvelope` (the un-invoiced
//! remainder); an ACL maps it into billing's `NewSalesInvoice` (adding the A/R + revenue accounts) →
//! billing raises + posts the invoice → **revenue post** (`Dr A/R · Cr Revenue`) into the REAL ledger
//! + a `SalesInvoicePosted{source_so_id, billed_lines}` event; an ACL routes it → selling
//! `mark_invoiced` → the order's `billed_qty` advances. Selling posts NO revenue itself.
//! Requires DATABASE_URL (:5433/backbone_selling with selling + billing + accounting migrated).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_events::{InvoiceRequestEnvelope, SellingEvent, SellingEventSink};
use backbone_selling::application::service::selling_write_service::{NewLine, NewSalesOrder, SellingError, SellingWriteService};

use backbone_billing::application::service::billing_events::{BillingEvent, BillingEventSink};
use backbone_billing::application::service::billing_gl::{
    AccountingPostEnvelope as BillEnv, GlPostAck as BillAck, GlPostRejected as BillRej, GlPostSink as BillSink,
};
use backbone_billing::application::service::billing_write_service::{
    BillingWriteService, NewInvoiceLine, NewSalesInvoice,
};

use backbone_accounting::application::service::posting_service::{PostingLine, PostingRequest, PostingService};

/// ACL: billing's serialized envelope → accounting's PostingRequest against the REAL ledger.
struct GlAdapter { svc: PostingService }
#[async_trait::async_trait]
impl BillSink for GlAdapter {
    async fn post(&self, e: &BillEnv) -> Result<BillAck, BillRej> {
        let mut r = PostingRequest::original(e.company_id, &e.source_type, e.source_id, e.posting_date);
        r.source_reference = e.source_reference.clone();
        r.lines = e.lines.iter().map(|l| PostingLine {
            account_id: l.account_id, debit: l.debit, credit: l.credit,
            party_type: l.party_type.clone(), party_id: l.party_id,
            cost_center_id: None, project_id: None, department_id: None, description: l.description.clone(),
        }).collect();
        match self.svc.post(r, None).await {
            Ok(x) => Ok(BillAck { post_id: x.post_id, journal_id: x.journal_id, idempotent_reuse: x.idempotent_reuse }),
            Err(x) => Err(BillRej { code: x.code().to_string(), message: x.to_string() }),
        }
    }
}

/// Records selling's `OrderInvoiced` so the test can route it → billing.
#[derive(Default, Clone)]
struct RecordingSellSink { events: Arc<Mutex<Vec<SellingEvent>>> }
impl SellingEventSink for RecordingSellSink {
    fn publish(&self, e: SellingEvent) { self.events.lock().unwrap().push(e); }
}
/// Records billing's `SalesInvoicePosted` so the test can route it → selling.
#[derive(Default, Clone)]
struct RecordingBillSink { events: Arc<Mutex<Vec<BillingEvent>>> }
impl BillingEventSink for RecordingBillSink {
    fn publish(&self, e: BillingEvent) { self.events.lock().unwrap().push(e); }
}

fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }
fn day() -> chrono::NaiveDate { chrono::NaiveDate::from_ymd_opt(2026, 7, 5).unwrap() }
fn uq(p: &str) -> String { format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8]) }
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}
async fn seed_coa(pool: &PgPool) -> (Uuid, HashMap<&'static str, Uuid>) {
    let company = Uuid::new_v4();
    let coa: &[(&str, &str, &str, &str, &str)] = &[
        ("1200", "Piutang Usaha", "asset", "accounts_receivable", "debit"),
        ("4000", "Pendapatan", "revenue", "operating_revenue", "credit"),
    ];
    let mut m = HashMap::new();
    for (code, name, at, st, nb) in coa {
        let id = Uuid::new_v4();
        sqlx::query(r#"INSERT INTO accounting.accounts (id, company_id, account_number, account_code, name, account_type, account_subtype, normal_balance, is_header, is_detail, status)
            VALUES ($1,$2,$3,$4,$5,$6::account_type,$7::account_subtype,$8::normal_balance,false,true,'active'::account_status)"#)
            .bind(id).bind(company).bind(code).bind(code).bind(name).bind(at).bind(st).bind(nb)
            .execute(pool).await.expect("seed acct");
        m.insert(*code, id);
    }
    (company, m)
}
async fn journal_totals(pool: &PgPool, jid: Uuid) -> (Decimal, Decimal) {
    let r = sqlx::query("SELECT total_debit, total_credit FROM accounting.journals WHERE id=$1").bind(jid).fetch_one(pool).await.unwrap();
    (r.get("total_debit"), r.get("total_credit"))
}
fn line(item: Uuid, qty: &str) -> NewLine {
    NewLine { item_id: item, revenue_account_id: None, description: None, quantity: d(qty), unit_price: d("100000"), line_discount: Decimal::ZERO }
}
async fn confirmed_order(selling: &SellingWriteService, company: Uuid, lines: Vec<NewLine>) -> Uuid {
    let order = selling.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, company_id: company, branch_id: None, customer_id: Uuid::new_v4(),
        order_date: day(), delivery_date: None, currency: None, tax_rate: Decimal::ZERO, notes: None, lines,
    }).await.unwrap();
    selling.confirm_sales_order(order, company).await.unwrap();
    order
}
async fn billed_total(pool: &PgPool, order: Uuid) -> Decimal {
    sqlx::query_scalar("SELECT COALESCE(SUM(billed_qty),0) FROM selling.sales_order_items WHERE order_id=$1").bind(order).fetch_one(pool).await.unwrap()
}

// ISEAM-2 (council 2026-07-05): `mark_invoiced` is BOUNDED — you cannot bill past the ordered
// quantity. A repeat/racy invoice (billed_qty advances only at post time, so two requests can both
// ask the full remainder) is refused at the writer, so `billed_qty` never exceeds `quantity` and no
// revenue is booked beyond the order. Without the cap the second call runs billed_qty to 20 on a 10.
#[tokio::test]
async fn over_billing_is_refused() {
    let pool = pool().await;
    let selling = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&selling, company, vec![line(item, "10")]).await;

    selling.mark_invoiced(order, &[(item, d("10"))]).await.unwrap();
    assert_eq!(billed_total(&pool, order).await, d("10.0000"));
    // a second full invoice against the same order is refused — the order is fully billed.
    let e = selling.mark_invoiced(order, &[(item, d("10"))]).await.unwrap_err();
    assert!(matches!(e, SellingError::OverBilled));
    assert_eq!(billed_total(&pool, order).await, d("10.0000"), "a rejected mark_invoiced leaves the watermark untouched");
}

// ISEAM-3 (council 2026-07-05): the aggregate-by-item allocate is correct for duplicate-item orders —
// two lines of item X (6 + 4) have total capacity 10; billing 12 is refused, billing 10 fills both.
#[tokio::test]
async fn duplicate_item_lines_allocate_by_capacity() {
    let pool = pool().await;
    let selling = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&selling, company, vec![line(item, "6"), line(item, "4")]).await;

    // 12 > total capacity 10 → refused, nothing advances.
    assert!(matches!(selling.mark_invoiced(order, &[(item, d("12"))]).await.unwrap_err(), SellingError::OverBilled));
    assert_eq!(billed_total(&pool, order).await, d("0.0000"));
    // 10 fills both lines to their caps.
    selling.mark_invoiced(order, &[(item, d("10"))]).await.unwrap();
    let caps: Vec<Decimal> = sqlx::query_scalar("SELECT billed_qty FROM selling.sales_order_items WHERE order_id=$1 ORDER BY quantity DESC").bind(order).fetch_all(&pool).await.unwrap();
    assert_eq!(caps, vec![d("6.0000"), d("4.0000")]);
}

/// ISEAM-1: order-to-cash billing across selling, billing, and the real ledger.
#[tokio::test]
async fn order_invoiced_across_three_modules() {
    let pool = pool().await;
    let (company, coa) = seed_coa(&pool).await;
    let customer = Uuid::new_v4();
    let item = Uuid::new_v4();

    let sell_rec = RecordingSellSink::default();
    let selling = SellingWriteService::with_sink(pool.clone(), Arc::new(sell_rec.clone()));
    let bill_rec = RecordingBillSink::default();
    let billing = BillingWriteService::with_sink(pool.clone(), Arc::new(bill_rec.clone()));
    let gl = GlAdapter { svc: PostingService::new(pool.clone()) };

    // 1) selling: create + confirm a Sales Order — 10 @ 100,000 (no tax).
    let order = selling.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, company_id: company, branch_id: None, customer_id: customer,
        order_date: day(), delivery_date: None, currency: None, tax_rate: Decimal::ZERO, notes: None,
        lines: vec![NewLine { item_id: item, revenue_account_id: None, description: None, quantity: d("10"), unit_price: d("100000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();
    selling.confirm_sales_order(order, company).await.unwrap();

    // 2) selling emits the invoice request (un-invoiced remainder = 10).
    let req: InvoiceRequestEnvelope = selling.build_invoice_request(order).await.unwrap();
    assert_eq!(req.lines.len(), 1);
    assert_eq!(req.lines[0].quantity, d("10.0000"));

    // 3) ACL: map the request into billing's NewSalesInvoice (adding A/R + revenue accounts) → post.
    let inv = billing.create_sales_invoice(NewSalesInvoice {
        invoice_number: uq("SI"), company_id: req.company_id, branch_id: None, customer_id: req.customer_id,
        source_so_id: Some(req.order_id), posting_date: day(), due_date: None, currency: None,
        receivable_account_id: coa["1200"],
        lines: req.lines.iter().map(|l| NewInvoiceLine {
            item_id: l.item_id, account_id: coa["4000"], description: None, quantity: l.quantity, unit_price: l.unit_price,
        }).collect(),
        tax_lines: vec![],
    }).await.unwrap();
    let out = billing.post_sales_invoice(inv, &gl).await.unwrap();
    // Revenue journal: Dr A/R 1,000,000 · Cr Revenue 1,000,000.
    assert_eq!(journal_totals(&pool, out.journal_id).await, (d("1000000"), d("1000000")));

    // 4) ACL: SalesInvoicePosted (source_so_id = our order) → selling.mark_invoiced.
    let posted = bill_rec.events.lock().unwrap().iter().find_map(|e| match e {
        BillingEvent::SalesInvoicePosted(p) if p.source_so_id == Some(order) => Some(p.clone()), _ => None,
    }).expect("SalesInvoicePosted for our order");
    assert_eq!(posted.grand_total, d("1000000.00"));
    let billed: Vec<(Uuid, Decimal)> = posted.billed_lines.iter().map(|l| (l.item_id, l.quantity)).collect();
    assert_eq!(billed, vec![(item, d("10.0000"))]);
    selling.mark_invoiced(order, &billed).await.unwrap();

    // 5) the order's billed watermark advanced via a REAL billing invoice (not a simulated leg).
    let bq: Decimal = sqlx::query_scalar("SELECT billed_qty FROM selling.sales_order_items WHERE order_id=$1").bind(order).fetch_one(&pool).await.unwrap();
    assert_eq!(bq, d("10.0000"));
    // selling's own SalesInvoicePosted was NOT emitted — selling posted no revenue itself.
    assert!(!sell_rec.events.lock().unwrap().iter().any(|e| matches!(e, SellingEvent::SalesInvoicePosted(_))), "selling posts no revenue in the composed flow");
    // billing's sales invoice is posted + linked to the order.
    let (ps, so): (String, Option<Uuid>) = sqlx::query_as("SELECT posting_state::text, source_so_id FROM billing.sales_invoices WHERE id=$1").bind(inv).fetch_one(&pool).await.unwrap();
    assert_eq!(ps, "posted");
    assert_eq!(so, Some(order));
}
