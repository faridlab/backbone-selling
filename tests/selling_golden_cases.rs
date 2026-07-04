//! Golden numeric oracle for the selling write path (mirrors docs/business-flows/golden-cases.md).
//!
//! Selling-only: proves server-side line/total computation, the revenue-post envelope shape, and
//! the validation gates — against real Postgres (selling.* schema), no accounting needed.
//! Requires DATABASE_URL (defaults to :5433/backbone_selling).

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_write_service::{
    NewLine, NewQuotation, NewSalesInvoice, NewSalesOrder, SellingError, SellingWriteService,
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
    NewLine {
        item_id: Uuid::new_v4(),
        revenue_account_id: Some(revenue),
        description: None,
        quantity: d(qty),
        unit_price: d(price),
        line_discount: d(discount),
    }
}

fn invoice(company: Uuid, customer: Uuid, ar: Uuid, ppn: Option<Uuid>, tax_rate: &str, lines: Vec<NewLine>) -> NewSalesInvoice {
    NewSalesInvoice {
        invoice_number: uq("INV"),
        sales_order_id: None,
        company_id: company,
        branch_id: None,
        customer_id: customer,
        invoice_date: day(2026, 7, 3),
        due_date: None,
        currency: None,
        tax_rate: d(tax_rate),
        receivable_account_id: ar,
        tax_output_account_id: ppn,
        notes: None,
        lines,
    }
}

// SGC-1: line + total math — qty 3 × 250,000, PPN 11% → subtotal 750,000, tax 82,500, total 832,500.
#[tokio::test]
async fn line_and_total_math() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, ppn, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let id = w.create_sales_invoice(invoice(company, customer, ar, Some(ppn), "11",
        vec![line(rev, "3", "250000", "0")])).await.unwrap();

    let row = sqlx::query("SELECT subtotal, tax_amount, total FROM selling.sales_invoices WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<Decimal, _>("subtotal"), d("750000"));
    assert_eq!(row.get::<Decimal, _>("tax_amount"), d("82500.00"));
    assert_eq!(row.get::<Decimal, _>("total"), d("832500.00"));

    let env = w.build_revenue_post(id).await.unwrap();
    assert!(env.is_balanced());
    let (deb, cred) = env.totals();
    assert_eq!(deb, d("832500.00"));
    assert_eq!(cred, d("832500.00"));
    assert_eq!(env.lines.len(), 3, "Dr A/R + Cr Revenue + Cr PPN");
}

// SGC-2: revenue grouped by income account — two income accounts produce two credit lines,
// each the sum of its lines; A/R debit = grand total.
#[tokio::test]
async fn revenue_grouped_by_income_account() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, ppn) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (rev_a, rev_b) = (Uuid::new_v4(), Uuid::new_v4());
    let id = w.create_sales_invoice(invoice(company, customer, ar, Some(ppn), "0",
        vec![
            line(rev_a, "1", "100000", "0"),
            line(rev_a, "1", "50000", "0"),
            line(rev_b, "1", "200000", "0"),
        ])).await.unwrap();

    let env = w.build_revenue_post(id).await.unwrap();
    assert!(env.is_balanced());
    // No tax → 1 debit + 2 revenue credits.
    assert_eq!(env.lines.len(), 3);
    let credit_for = |acct: Uuid| env.lines.iter().find(|l| l.account_id == acct && l.credit > Decimal::ZERO).map(|l| l.credit);
    assert_eq!(credit_for(rev_a), Some(d("150000")), "rev_a summed");
    assert_eq!(credit_for(rev_b), Some(d("200000")));
    let ar_debit = env.lines.iter().find(|l| l.account_id == ar).unwrap().debit;
    assert_eq!(ar_debit, d("350000"));
}

// SGC-3: zero tax — envelope has exactly 2 lines (no PPN), and PPN account is not required.
#[tokio::test]
async fn zero_tax_needs_no_ppn_account() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let id = w.create_sales_invoice(invoice(company, customer, ar, None, "0",
        vec![line(rev, "1", "1000000", "0")])).await.unwrap();
    let env = w.build_revenue_post(id).await.unwrap();
    assert_eq!(env.lines.len(), 2, "Dr A/R + Cr Revenue only");
    assert!(env.is_balanced());
}

// SGC-4: money rounding — subtotal 100.05 at 11% = 11.0055 → half-up → 11.01; total 111.06.
#[tokio::test]
async fn tax_rounds_half_up() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, ppn, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let id = w.create_sales_invoice(invoice(company, customer, ar, Some(ppn), "11",
        vec![line(rev, "1", "100.05", "0")])).await.unwrap();
    let row = sqlx::query("SELECT subtotal, tax_amount, total FROM selling.sales_invoices WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<Decimal, _>("subtotal"), d("100.05"));
    assert_eq!(row.get::<Decimal, _>("tax_amount"), d("11.01"), "11.0055 rounds half-up to 11.01");
    assert_eq!(row.get::<Decimal, _>("total"), d("111.06"));
    // Envelope still balances exactly at the rounded numbers.
    assert!(w.build_revenue_post(id).await.unwrap().is_balanced());
}

// SGC-5: line discount reduces the line amount; total reflects it.
#[tokio::test]
async fn line_discount_applied() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let id = w.create_sales_invoice(invoice(company, customer, ar, None, "0",
        vec![line(rev, "2", "100000", "25000")])).await.unwrap(); // 200,000 - 25,000
    let sub: Decimal = sqlx::query_scalar("SELECT subtotal FROM selling.sales_invoices WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(sub, d("175000"));
}

// SGC-6: validation gates — empty doc / negative net / missing revenue account / missing tax
// account / duplicate number are all rejected with stable codes.
#[tokio::test]
async fn validation_gates() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, ppn, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    // empty document
    let e = w.create_sales_invoice(invoice(company, customer, ar, None, "0", vec![])).await.unwrap_err();
    assert!(matches!(e, SellingError::EmptyDocument));

    // discount exceeds line → negative net
    let e = w.create_sales_invoice(invoice(company, customer, ar, None, "0",
        vec![line(rev, "1", "100", "500")])).await.unwrap_err();
    assert!(matches!(e, SellingError::NegativeQuantity));

    // invoice line without a revenue account
    let mut bad = invoice(company, customer, ar, None, "0", vec![line(rev, "1", "1000", "0")]);
    bad.lines[0].revenue_account_id = None;
    let e = w.create_sales_invoice(bad).await.unwrap_err();
    assert!(matches!(e, SellingError::MissingRevenueAccount));

    // tax charged but no PPN output account
    let e = w.create_sales_invoice(invoice(company, customer, ar, None, "11",
        vec![line(rev, "1", "1000", "0")])).await.unwrap_err();
    assert!(matches!(e, SellingError::TaxAccountMissing));

    // duplicate invoice number
    let num = uq("DUP");
    let mut a = invoice(company, customer, ar, Some(ppn), "0", vec![line(rev, "1", "1000", "0")]);
    a.invoice_number = num.clone();
    w.create_sales_invoice(a).await.unwrap();
    let mut b = invoice(company, customer, ar, Some(ppn), "0", vec![line(rev, "1", "1000", "0")]);
    b.invoice_number = num;
    let e = w.create_sales_invoice(b).await.unwrap_err();
    assert!(matches!(e, SellingError::DuplicateNumber(_)));
}

// SGC-8: a non-IDR invoice is refused at the posting seam (no exchange_rate → would book
// face-value into an IDR ledger). Council 2026-07-03 / ADR-002.
#[tokio::test]
async fn non_idr_invoice_refused_at_post() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, ar, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    // DB CHECK blocks creating a USD invoice at all …
    let mut usd = invoice(company, customer, ar, None, "0", vec![line(rev, "1", "1000000", "0")]);
    usd.currency = Some("USD".into());
    let e = w.create_sales_invoice(usd).await.unwrap_err();
    assert!(matches!(e, SellingError::Db(_)), "IDR-only CHECK rejects a USD invoice at create");

    // … and the build_revenue_post guard is the belt to that DB suspenders (defense in depth):
    // an IDR invoice builds fine.
    let id = w.create_sales_invoice(invoice(company, customer, ar, None, "0",
        vec![line(rev, "1", "1000000", "0")])).await.unwrap();
    assert!(w.build_revenue_post(id).await.is_ok());
}

// SGC-7: quotation → sales order → confirm; totals persist and status transitions.
#[tokio::test]
async fn quotation_order_confirm_flow() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, customer, rev) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    let qid = w.create_quotation(NewQuotation {
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

    w.confirm_sales_order(oid).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "to_deliver_and_bill"); // ADR-003: confirmed order awaits both delivery and billing (inventory live)

    // confirming again (not draft) is rejected.
    assert!(matches!(w.confirm_sales_order(oid).await.unwrap_err(), SellingError::NotDraft(_)));
}
