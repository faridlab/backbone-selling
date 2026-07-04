//! Order-to-cash conversion flow: Quotation → accept → Sales Order → confirm → Sales Invoice
//! (from the order) → post → billed watermarks advance and the order completes.
//!
//! This exercises the LINKAGE the council flagged as owed (Quote→Order→Invoice conversion +
//! billed_qty), on the selling side. The GL post uses a stub sink — the REAL ledger seam is proven
//! separately in `gl_posting_seam.rs`, so here we only need a successful ack to drive reconciliation.
//! Requires DATABASE_URL (:5433/backbone_selling).

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::selling_gl::{
    AccountingPostEnvelope, GlPostAck, GlPostRejected, GlPostSink,
};
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewQuotation, SellingWriteService,
};

// A stub GL sink: always accepts, returns fresh ids. (Real ledger behavior is proven elsewhere.)
struct StubGlSink;
#[async_trait::async_trait]
impl GlPostSink for StubGlSink {
    async fn post(&self, _env: &AccountingPostEnvelope) -> Result<GlPostAck, GlPostRejected> {
        Ok(GlPostAck { post_id: Uuid::new_v4(), journal_id: Uuid::new_v4(), idempotent_reuse: false })
    }
}

fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }
fn day(y: i32, m: u32, dd: u32) -> chrono::NaiveDate { chrono::NaiveDate::from_ymd_opt(y, m, dd).unwrap() }
fn uq(p: &str) -> String { format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8]) }
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}

// OTC-1: full Quote→Order→Invoice→post; quotation/order/invoice are LINKED, billed_qty advances,
// and the fully-billed order reaches `completed`.
#[tokio::test]
async fn quote_to_order_to_invoice_to_posted() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, rev, ppn) = (
        Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    // 1) Quotation: 10 × 100,000 @ PPN 11%.
    let qid = w.create_quotation(NewQuotation {
        quotation_number: uq("QUO"), company_id: company, branch_id: None, customer_id: customer,
        quotation_date: day(2026, 7, 1), valid_until: Some(day(2026, 7, 31)), currency: None,
        tax_rate: d("11"), notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: None, description: Some("Widget".into()),
            quantity: d("10"), unit_price: d("100000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();

    // 2) Accept, then 3) convert to a sales order.
    w.accept_quotation(qid).await.unwrap();
    let oid = w.convert_quotation_to_order(qid, uq("SO")).await.unwrap();

    // Linkage: order references the quotation; quotation is now `ordered`.
    let linked_q: Option<Uuid> = sqlx::query_scalar("SELECT quotation_id FROM selling.sales_orders WHERE id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(linked_q, Some(qid));
    let qstatus: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(qstatus, "ordered");

    // 4) Confirm the order → to_bill.
    w.confirm_sales_order(oid).await.unwrap();

    // 5) Raise the invoice FROM the order (lines linked to their SO lines).
    let inv = w.create_invoice_from_order(oid, uq("INV"), day(2026, 7, 3), ar, rev, Some(ppn)).await.unwrap();
    let (isub, itax, itotal, ilinked): (Decimal, Decimal, Decimal, Option<Uuid>) = {
        let row = sqlx::query_as::<_, (Decimal, Decimal, Decimal, Option<Uuid>)>(
            "SELECT subtotal, tax_amount, total, sales_order_id FROM selling.sales_invoices WHERE id=$1",
        ).bind(inv).fetch_one(&pool).await.unwrap();
        row
    };
    assert_eq!(isub, d("1000000"));
    assert_eq!(itax, d("110000.00"));
    assert_eq!(itotal, d("1110000.00"));
    assert_eq!(ilinked, Some(oid), "invoice links back to the order");
    // Each invoice line carries its source SO line.
    let linked_lines: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM selling.sales_invoice_items WHERE invoice_id=$1 AND sales_order_item_id IS NOT NULL")
        .bind(inv).fetch_one(&pool).await.unwrap();
    assert_eq!(linked_lines, 1);

    // 6) Post → billed_qty advances; order is billed but NOT yet delivered → to_deliver (ADR-003;
    //    the delivery seam advances delivered_qty and reaches completed — proven in delivery_seam.rs).
    w.post_sales_invoice(inv, &StubGlSink).await.unwrap();
    let billed: Decimal = sqlx::query_scalar(
        "SELECT billed_qty FROM selling.sales_order_items WHERE order_id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(billed, d("10.0000"), "billed_qty advanced by the invoiced qty");
    let ostatus: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(ostatus, "to_deliver", "fully billed but undelivered → to_deliver");

    // Deliver the full qty → order completes (both watermarks satisfied).
    w.mark_delivered(oid, &[(item_of(&pool, oid).await, d("10"))]).await.unwrap();
    let ostatus2: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(ostatus2, "completed", "billed + delivered → completed");
}

// helper: the item_id of an order's (single) line
async fn item_of(pool: &PgPool, order_id: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT item_id FROM selling.sales_order_items WHERE order_id=$1 LIMIT 1")
        .bind(order_id).fetch_one(pool).await.unwrap()
}

// OTC-2: converting a NON-accepted quotation is rejected (quotation_not_accepted).
#[tokio::test]
async fn convert_requires_accepted_quotation() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let qid = w.create_quotation(NewQuotation {
        quotation_number: uq("QUO"), company_id: company, branch_id: None, customer_id: customer,
        quotation_date: day(2026, 7, 1), valid_until: None, currency: None, tax_rate: Decimal::ZERO, notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: Some(rev), description: None,
            quantity: d("1"), unit_price: d("1000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();
    // not accepted yet
    let err = w.convert_quotation_to_order(qid, uq("SO")).await.unwrap_err();
    assert_eq!(err.code(), "quotation_not_accepted");
}
