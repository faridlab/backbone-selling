//! The marquee cross-module seam: **selling → accounting** via the GL-posting contract.
//!
//! Proves that a sales invoice, posted through selling's write path, records a balanced revenue
//! entry in the REAL `backbone-accounting` ledger — with NO horizontal library edge. The only
//! thing crossing the boundary is the serialized `AccountingPostEnvelope`; the in-test
//! `AccountingAdapter` maps it into accounting's `PostingRequest` (the ACL translation a composing
//! service would own) and calls the real `PostingService`. `backbone-accounting` is a
//! DEV-dependency here only.
//!
//! Both modules keep their own Postgres schema (`selling.*`, `accounting.*`) inside one database,
//! so a single connection serves both — exactly as a composed service would run them.
//! Requires DATABASE_URL (defaults to :5433/backbone_selling with both schemas migrated).

use std::collections::HashMap;

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_gl::{
    AccountingPostEnvelope, GlPostAck, GlPostRejected, GlPostSink,
};
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesInvoice, SellingWriteService,
};

use backbone_accounting::application::service::posting_service::{
    PostingLine, PostingRequest, PostingService,
};

// ── the ACL adapter: envelope → accounting PostingRequest → real PostingService ──────────────
struct AccountingAdapter {
    svc: PostingService,
}

#[async_trait::async_trait]
impl GlPostSink for AccountingAdapter {
    async fn post(&self, env: &AccountingPostEnvelope) -> Result<GlPostAck, GlPostRejected> {
        let mut req = PostingRequest::original(
            env.company_id,
            &env.source_type,
            env.source_id,
            env.posting_date,
        );
        req.branch_id = env.branch_id;
        req.source_reference = env.source_reference.clone();
        req.currency = env.currency.clone();
        req.description = env.description.clone();
        req.lines = env
            .lines
            .iter()
            .map(|l| PostingLine {
                account_id: l.account_id,
                debit: l.debit,
                credit: l.credit,
                party_type: l.party_type.clone(),
                party_id: l.party_id,
                cost_center_id: None,
                project_id: None,
                department_id: None,
                description: l.description.clone(),
            })
            .collect();
        match self.svc.post(req, None).await {
            Ok(r) => Ok(GlPostAck {
                post_id: r.post_id,
                journal_id: r.journal_id,
                idempotent_reuse: r.idempotent_reuse,
            }),
            Err(e) => Err(GlPostRejected { code: e.code().to_string(), message: e.to_string() }),
        }
    }
}

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

/// Seed a minimal chart of accounts in `accounting.*` under a fresh company. Returns
/// (company_id, code→id) with 1200 A/R, 2200 PPN Output, 4000 Revenue, plus a 1000 header.
async fn seed_coa(pool: &PgPool) -> (Uuid, HashMap<&'static str, Uuid>) {
    let company_id = Uuid::new_v4();
    let coa: &[(&str, &str, &str, &str, &str, bool, bool)] = &[
        ("1000", "Header Aset", "asset", "current_asset", "debit", true, false),
        ("1200", "Piutang Usaha", "asset", "accounts_receivable", "debit", false, true),
        ("2200", "PPN Keluaran", "liability", "tax", "credit", false, true),
        ("4000", "Pendapatan", "revenue", "operating_revenue", "credit", false, true),
        ("4100", "Pendapatan Jasa", "revenue", "operating_revenue", "credit", false, true),
    ];
    let mut map = HashMap::new();
    for (code, name, at, st, nb, is_header, is_detail) in coa {
        let id = Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO accounting.accounts
                (id, company_id, account_number, account_code, name, account_type, account_subtype,
                 normal_balance, is_header, is_detail, status)
               VALUES ($1,$2,$3,$4,$5,$6::account_type,$7::account_subtype,$8::normal_balance,
                       $9,$10,'active'::account_status)"#,
        )
        .bind(id).bind(company_id).bind(code).bind(code).bind(name)
        .bind(at).bind(st).bind(nb).bind(is_header).bind(is_detail)
        .execute(pool).await.expect("seed account");
        map.insert(*code, id);
    }
    (company_id, map)
}

async fn journal_count(pool: &PgPool, company: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM accounting.journals WHERE company_id=$1")
        .bind(company).fetch_one(pool).await.unwrap()
}

/// SEAM-1: exclusive PPN 11% invoice for 1,000,000 posts Dr A/R 1,110,000 · Cr Revenue 1,000,000
/// · Cr PPN Output 110,000 into the real GL; selling reconciles to posted.
#[tokio::test]
async fn revenue_post_lands_balanced_in_the_real_gl() {
    let pool = pool().await;
    let (company, coa) = seed_coa(&pool).await;
    let write = SellingWriteService::new(pool.clone());
    let adapter = AccountingAdapter { svc: PostingService::new(pool.clone()) };
    let customer = Uuid::new_v4();

    let invoice_id = write.create_sales_invoice(NewSalesInvoice {
        invoice_number: uq("INV"),
        sales_order_id: None,
        company_id: company,
        branch_id: None,
        customer_id: customer,
        invoice_date: day(2026, 7, 3),
        due_date: None,
        currency: None,
        tax_rate: d("11"),
        receivable_account_id: coa["1200"],
        tax_output_account_id: Some(coa["2200"]),
        notes: None,
        lines: vec![NewLine {
            item_id: Uuid::new_v4(),
            revenue_account_id: Some(coa["4000"]),
            description: Some("Widget".into()),
            quantity: d("1"),
            unit_price: d("1000000"),
            line_discount: Decimal::ZERO,
        }],
    }).await.expect("create invoice");

    let outcome = write.post_sales_invoice(invoice_id, &adapter).await.expect("post");
    assert!(!outcome.idempotent_reuse, "first post is fresh");

    // The journal balances at the grand total.
    let jrow = sqlx::query(
        "SELECT total_debit, total_credit, line_count FROM accounting.journals WHERE id=$1",
    )
    .bind(outcome.journal_id).fetch_one(&pool).await.unwrap();
    let td: Decimal = jrow.get("total_debit");
    let tc: Decimal = jrow.get("total_credit");
    assert_eq!(td, d("1110000"), "debit total");
    assert_eq!(tc, d("1110000"), "credit total");
    assert_eq!(jrow.get::<i32, _>("line_count"), 3);

    // A/R debit carries the customer party (subledger aging).
    let ar_debit: Decimal = sqlx::query_scalar(
        "SELECT debit_amount FROM accounting.journal_lines WHERE journal_id=$1 AND account_id=$2",
    )
    .bind(outcome.journal_id).bind(coa["1200"]).fetch_one(&pool).await.unwrap();
    assert_eq!(ar_debit, d("1110000"));
    let ar_party: Option<Uuid> = sqlx::query_scalar(
        "SELECT party_id FROM accounting.journal_lines WHERE journal_id=$1 AND account_id=$2",
    )
    .bind(outcome.journal_id).bind(coa["1200"]).fetch_one(&pool).await.unwrap();
    assert_eq!(ar_party, Some(customer), "A/R line carries the customer party");

    // Revenue + PPN credits.
    let rev_credit: Decimal = sqlx::query_scalar(
        "SELECT credit_amount FROM accounting.journal_lines WHERE journal_id=$1 AND account_id=$2",
    )
    .bind(outcome.journal_id).bind(coa["4000"]).fetch_one(&pool).await.unwrap();
    assert_eq!(rev_credit, d("1000000"), "revenue credit = subtotal");
    let ppn_credit: Decimal = sqlx::query_scalar(
        "SELECT credit_amount FROM accounting.journal_lines WHERE journal_id=$1 AND account_id=$2",
    )
    .bind(outcome.journal_id).bind(coa["2200"]).fetch_one(&pool).await.unwrap();
    assert_eq!(ppn_credit, d("110000"), "PPN Output credit = tax");

    // Selling side reconciled to posted.
    let row = sqlx::query(
        "SELECT posting_state::text AS ps, status::text AS st, journal_id, accounting_post_id, outstanding_amount \
         FROM selling.sales_invoices WHERE id=$1",
    )
    .bind(invoice_id).fetch_one(&pool).await.unwrap();
    assert_eq!(row.get::<String, _>("ps"), "posted");
    assert_eq!(row.get::<String, _>("st"), "submitted");
    assert_eq!(row.get::<Option<Uuid>, _>("journal_id"), Some(outcome.journal_id));
    assert_eq!(row.get::<Option<Uuid>, _>("accounting_post_id"), Some(outcome.post_id));
    assert_eq!(row.get::<Decimal, _>("outstanding_amount"), d("1110000"));
}

/// SEAM-2: posting the same invoice twice is idempotent — one journal, the second call replays
/// the recorded ids (no double revenue in the GL).
#[tokio::test]
async fn reposting_is_idempotent() {
    let pool = pool().await;
    let (company, coa) = seed_coa(&pool).await;
    let write = SellingWriteService::new(pool.clone());
    let adapter = AccountingAdapter { svc: PostingService::new(pool.clone()) };

    let invoice_id = write.create_sales_invoice(NewSalesInvoice {
        invoice_number: uq("INV"), sales_order_id: None, company_id: company, branch_id: None,
        customer_id: Uuid::new_v4(), invoice_date: day(2026, 7, 3), due_date: None, currency: None,
        tax_rate: d("11"), receivable_account_id: coa["1200"], tax_output_account_id: Some(coa["2200"]),
        notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: Some(coa["4000"]),
            description: None, quantity: d("2"), unit_price: d("500000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();

    let first = write.post_sales_invoice(invoice_id, &adapter).await.unwrap();
    assert!(!first.idempotent_reuse);
    let second = write.post_sales_invoice(invoice_id, &adapter).await.unwrap();
    assert!(second.idempotent_reuse, "second post replays");
    assert_eq!(first.journal_id, second.journal_id, "same journal");
    assert_eq!(journal_count(&pool, company).await, 1, "exactly one journal for the company");
}

/// SEAM-3: a rejection from accounting (non-postable header account) surfaces the GL's stable code
/// and leaves the invoice `failed`, not posted — the cross-seam failure path.
#[tokio::test]
async fn gl_rejection_marks_invoice_failed() {
    let pool = pool().await;
    let (company, coa) = seed_coa(&pool).await;
    let write = SellingWriteService::new(pool.clone());
    let adapter = AccountingAdapter { svc: PostingService::new(pool.clone()) };

    // Point A/R at a HEADER account (1000) — accounting must reject as non-postable.
    let invoice_id = write.create_sales_invoice(NewSalesInvoice {
        invoice_number: uq("INV"), sales_order_id: None, company_id: company, branch_id: None,
        customer_id: Uuid::new_v4(), invoice_date: day(2026, 7, 3), due_date: None, currency: None,
        tax_rate: Decimal::ZERO, receivable_account_id: coa["1000"], tax_output_account_id: None,
        notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: Some(coa["4000"]),
            description: None, quantity: d("1"), unit_price: d("1000000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();

    let err = write.post_sales_invoice(invoice_id, &adapter).await.unwrap_err();
    assert_eq!(err.code(), "non_postable_account", "surfaces the GL's stable code");

    let state: String = sqlx::query_scalar(
        "SELECT posting_state::text FROM selling.sales_invoices WHERE id=$1",
    )
    .bind(invoice_id).fetch_one(&pool).await.unwrap();
    assert_eq!(state, "failed");
    assert_eq!(journal_count(&pool, company).await, 0, "no journal written on rejection");
}

/// SEAM-4 (skeptic probe): TWO concurrent posts of the same invoice must yield exactly ONE journal.
#[tokio::test]
async fn concurrent_double_post_yields_one_journal() {
    let pool = pool().await;
    let (company, coa) = seed_coa(&pool).await;
    let write = SellingWriteService::new(pool.clone());
    let customer = Uuid::new_v4();
    let invoice_id = write.create_sales_invoice(NewSalesInvoice {
        invoice_number: uq("INV"), sales_order_id: None, company_id: company, branch_id: None,
        customer_id: customer, invoice_date: day(2026, 7, 3), due_date: None, currency: None,
        tax_rate: d("11"), receivable_account_id: coa["1200"], tax_output_account_id: Some(coa["2200"]),
        notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: Some(coa["4000"]),
            description: None, quantity: d("1"), unit_price: d("1000000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();

    let (w1, w2) = (write.clone(), write.clone());
    let (p1, p2) = (pool.clone(), pool.clone());
    let id1 = invoice_id;
    let a = tokio::spawn(async move {
        let adapter = AccountingAdapter { svc: PostingService::new(p1) };
        w1.post_sales_invoice(id1, &adapter).await
    });
    let b = tokio::spawn(async move {
        let adapter = AccountingAdapter { svc: PostingService::new(p2) };
        w2.post_sales_invoice(id1, &adapter).await
    });
    let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
    assert!(ra.is_ok() && rb.is_ok(), "both calls succeed: {ra:?} {rb:?}");
    let n = journal_count(&pool, company).await;
    println!("CONCURRENT_JOURNAL_COUNT={n}");
    assert_eq!(n, 1, "exactly one journal for one invoice under concurrency");
}
