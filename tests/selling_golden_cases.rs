//! Golden numeric oracle for the selling write path (mirrors docs/business-flows/golden-cases.md).
//!
//! Selling-only: proves server-side line/total computation + the order status transitions — against
//! real Postgres (selling.* schema), no accounting needed. Requires DATABASE_URL (:5433).
//!
//! The seven SALES-INVOICE golden cases (line/total math, revenue grouping, tax rounding, validation
//! gates, IDR guard) moved to `backbone-billing` when selling exited the invoice business (ADR-006):
//! `backbone-billing/tests/billing_golden_cases.rs` + `ar_seam.rs` now own that coverage.

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::selling_write_service::{
    NewLine, NewQuotation, NewSalesOrder, SellingError, SellingWriteService,
};

fn d(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}
fn day(y: i32, m: u32, dd: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(y, m, dd).unwrap()
}
fn uq(p: &str) -> String {
    format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8])
}
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}
fn line(revenue: Uuid, qty: &str, price: &str, discount: &str) -> NewLine {
    NewLine { invoice_policy: None, is_downpayment: None,
        item_id: Uuid::new_v4(),
        revenue_account_id: Some(revenue),
        description: None,
        quantity: d(qty),
        unit_price: d(price),
        line_discount: d(discount),
    }
}

// SGC-7: quotation → sales order → confirm; totals persist and status transitions.
#[tokio::test]
async fn quotation_order_confirm_flow() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    let qid = w.create_quotation(NewQuotation { opportunity_id: None, template_id: None,
        quotation_number: uq("QUO"), company_id: company, branch_id: None, customer_id: customer,
        quotation_date: day(2026, 7, 1), valid_until: Some(day(2026, 7, 31)), currency: None,
        tax_rate: d("11"), notes: None,
        lines: vec![line(rev, "10", "100000", "0")],
    }).await.unwrap();
    let qtotal: Decimal = sqlx::query_scalar("SELECT total FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(qtotal, d("1110000.00")); // 1,000,000 + 11%

    let oid = w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: Some(qid), company_id: company, branch_id: None,
        customer_id: customer, order_date: day(2026, 7, 2), delivery_date: None, currency: None,
        tax_rate: d("11"), notes: None,
        lines: vec![line(rev, "10", "100000", "0")],
    }).await.unwrap();

    w.confirm_sales_order(oid, company).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "to_deliver_and_bill"); // ADR-003: confirmed order awaits both delivery and billing (inventory live)

    // confirming again (not draft) is rejected.
    assert!(matches!(w.confirm_sales_order(oid, company).await.unwrap_err(), SellingError::NotDraft(_)));
}
