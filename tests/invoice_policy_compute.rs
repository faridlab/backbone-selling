//! Invoicing-policy engine goldens: the PURE COMPUTE (read-time `qty_to_invoice` /
//! `invoice_status`), the single-source guarantee across the SQL seam sites, the downpayment
//! rules, the quotation-template master, the opportunity link, the order-line freeze, and the
//! order cancel guard.
//!
//! The canonical basis is ONE expression mirrored in `list_billing_remainders`,
//! `lock_billing_capacity`, `watermark_rollup` (SQL) and `selling_invoice_policy.rs` (Rust):
//! `policy_base = (invoice_policy='delivery' AND NOT is_downpayment) ? delivered_qty : quantity`.
//! These tests prove the mirrors agree — the stranding defect this design closes is a
//! delivery-policy line whose order strands in `to_deliver_and_bill` when `billed_qty` reaches
//! only `delivered_qty` while the rollup still compares against `quantity`.
//!
//! Requires DATABASE_URL pointing at a Postgres with the selling schema migrated.

use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::selling_events::{SellingEvent, SellingEventSink};
use backbone_selling::application::service::selling_order::UpdateOrderLinePatch;
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewQuotation, NewSalesOrder, SellingError, SellingWriteService,
};
use backbone_selling::domain::entity::InvoicePolicy;

#[derive(Default, Clone)]
struct RecordingSink {
    events: Arc<Mutex<Vec<SellingEvent>>>,
}
impl SellingEventSink for RecordingSink {
    fn publish(&self, e: SellingEvent) {
        self.events.lock().unwrap().push(e);
    }
}
impl RecordingSink {
    fn has<F: Fn(&SellingEvent) -> bool>(&self, f: F) -> bool {
        self.events.lock().unwrap().iter().any(f)
    }
}

fn d(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}
fn uq(p: &str) -> String {
    format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8])
}
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}
fn pline(item: Uuid, qty: &str, policy: InvoicePolicy, downpayment: bool) -> NewLine {
    NewLine {
        invoice_policy: Some(policy),
        is_downpayment: Some(downpayment),
        item_id: item,
        revenue_account_id: None,
        description: None,
        quantity: d(qty),
        unit_price: d("100000"),
        line_discount: Decimal::ZERO,
    }
}
async fn confirmed_order(w: &SellingWriteService, company: Uuid, lines: Vec<NewLine>) -> Uuid {
    let order = w
        .create_sales_order(NewSalesOrder {
            order_number: uq("SO"),
            quotation_id: None,
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            delivery_date: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines,
        })
        .await
        .unwrap();
    w.confirm_sales_order(order, company).await.unwrap();
    order
}
async fn line_id(pool: &PgPool, order: Uuid, item: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM selling.sales_order_items WHERE order_id=$1 AND item_id=$2")
        .bind(order)
        .bind(item)
        .fetch_one(pool)
        .await
        .unwrap()
}
fn n(v: Decimal) -> Decimal {
    v.normalize()
}

// ── PC: policy compute goldens ─────────────────────────────────────────────────

// PC-1: an order-policy line's invoiceable quantity is `quantity − billed_qty`.
#[tokio::test]
async fn order_policy_bills_on_ordered_quantity() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Order, false)]).await;

    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.invoice_status, "to invoice");
    assert_eq!(n(view.lines[0].qty_to_invoice), d("10"));
    assert_eq!(view.lines[0].invoice_status, "to invoice");

    w.mark_invoiced(order, company, &[(item, d("4"))]).await.unwrap();
    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(n(view.lines[0].qty_to_invoice), d("6"));
    assert_eq!(view.lines[0].invoice_status, "to invoice");

    w.mark_invoiced(order, company, &[(item, d("6"))]).await.unwrap();
    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.invoice_status, "invoiced");
    assert_eq!(view.lines[0].invoice_status, "invoiced");
    assert_eq!(n(view.lines[0].qty_to_invoice), d("0"));
}

// PC-2: a delivery-policy line's invoiceable quantity is `delivered_qty − billed_qty` — undelivered
// quantity is NOT invoiceable even though the order is confirmed.
#[tokio::test]
async fn delivery_policy_bills_on_delivered_quantity() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Delivery, false)]).await;

    // Nothing delivered: nothing invoiceable, and the writer refuses any bill (capacity is 0).
    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.lines[0].invoice_status, "no");
    assert_eq!(n(view.lines[0].qty_to_invoice), d("0"));
    assert!(matches!(
        w.mark_invoiced(order, company, &[(item, d("1"))]).await.unwrap_err(),
        SellingError::OverBilled
    ));

    // 6 delivered: exactly 6 invoiceable, and billing it leaves the line fully billed-to-delivered.
    w.mark_delivered(order, company, &[(item, d("6"))]).await.unwrap();
    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(n(view.lines[0].qty_to_invoice), d("6"));
    assert_eq!(view.lines[0].invoice_status, "to invoice");

    w.mark_invoiced(order, company, &[(item, d("6"))]).await.unwrap();
    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.lines[0].invoice_status, "invoiced", "billed for everything deliverable so far");
    assert_eq!(view.invoice_status, "invoiced");
    // more delivery lands → the line reopens.
    w.mark_delivered(order, company, &[(item, d("4"))]).await.unwrap();
    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.lines[0].invoice_status, "to invoice");
    assert_eq!(n(view.lines[0].qty_to_invoice), d("4"));
}

// PC-3 (the stranding fix): a partially delivered + fully billed-to-delivered delivery-policy line
// does NOT strand the order in `to_deliver_and_bill` — the watermark rollup compares against the
// SAME policy basis, so the billing band is satisfied and the order advances to `to_deliver`.
#[tokio::test]
async fn delivery_policy_does_not_strand_the_order() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Delivery, false)]).await;

    w.mark_delivered(order, company, &[(item, d("6"))]).await.unwrap();
    w.mark_invoiced(order, company, &[(item, d("6"))]).await.unwrap();

    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "to_deliver", "billing band satisfied on the delivered basis; only delivery remains");
}

// PC-4: upselling — billed beyond the ordered quantity reads as `upselling` with a NEGATIVE
// qty_to_invoice. The seam writer can never produce this (it is capacity-bounded); the state is
// forced directly here to prove the READ model reports it instead of hiding it.
#[tokio::test]
async fn billed_beyond_ordered_reads_as_upselling() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Order, false)]).await;
    let lid = line_id(&pool, order, item).await;
    sqlx::query("UPDATE selling.sales_order_items SET billed_qty=12 WHERE id=$1")
        .bind(lid)
        .execute(&pool)
        .await
        .unwrap();

    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.lines[0].invoice_status, "upselling");
    assert_eq!(n(view.lines[0].qty_to_invoice), d("-2"));
    assert_eq!(view.invoice_status, "upselling");
}

// PC-5: a downpayment line stays on the QUANTITY basis (billing's advances precede delivery) but
// is EXCLUDED from the order-level aggregate and the status rollup.
#[tokio::test]
async fn downpayment_bills_on_quantity_but_is_excluded_from_aggregates() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, goods, dp) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(
        &w,
        company,
        vec![
            pline(goods, "10", InvoicePolicy::Delivery, false), // delivered 0 → nothing due yet
            pline(dp, "1", InvoicePolicy::Delivery, true),      // downpayment: quantity basis
        ],
    )
    .await;

    // The downpayment line IS invoiceable on its quantity basis despite zero delivery…
    let view = w.order_invoice_view(order).await.unwrap();
    let dp_line = view.lines.iter().find(|l| l.item_id == dp).unwrap();
    assert_eq!(dp_line.invoice_status, "to invoice");
    assert_eq!(n(dp_line.qty_to_invoice), d("1"));
    // …but the aggregate ignores it entirely (the goods line is `no` → aggregate `no`).
    assert_eq!(view.invoice_status, "no");
    // The writer honors the quantity basis for the downpayment even under a delivery policy.
    w.mark_invoiced(order, company, &[(dp, d("1"))]).await.unwrap();

    // The rollup excludes it too: delivering + billing the GOODS line completes the order even
    // though the downpayment line (quantity 1) is never delivered.
    w.mark_delivered(order, company, &[(goods, d("10"))]).await.unwrap();
    w.mark_invoiced(order, company, &[(goods, d("10"))]).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "completed");
}

// PC-6: the aggregate is actionable-first — a `to invoice` line outranks an `upselling` line.
#[tokio::test]
async fn aggregate_to_invoice_outranks_upselling() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(
        &w,
        company,
        vec![pline(a, "10", InvoicePolicy::Order, false), pline(b, "10", InvoicePolicy::Order, false)],
    )
    .await;
    let la = line_id(&pool, order, a).await;
    // line a billed past its order (upselling); line b untouched (to invoice).
    sqlx::query("UPDATE selling.sales_order_items SET billed_qty=11 WHERE id=$1")
        .bind(la)
        .execute(&pool)
        .await
        .unwrap();

    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.lines.iter().find(|l| l.item_id == a).unwrap().invoice_status, "upselling");
    assert_eq!(view.lines.iter().find(|l| l.item_id == b).unwrap().invoice_status, "to invoice");
    assert_eq!(view.invoice_status, "to invoice", "the actionable line wins the aggregate");
}

// PC-7: a draft (or cancelled) order's lines all read `no` — nothing is invoiceable before
// confirmation.
#[tokio::test]
async fn unconfirmed_orders_read_no() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = w
        .create_sales_order(NewSalesOrder {
            order_number: uq("SO"),
            quotation_id: None,
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            delivery_date: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines: vec![pline(item, "10", InvoicePolicy::Order, false)],
        })
        .await
        .unwrap();

    let view = w.order_invoice_view(order).await.unwrap();
    assert_eq!(view.lines[0].invoice_status, "no");
    assert_eq!(view.invoice_status, "no");
}

// PC-8: the quotation read model surfaces the policy + downpayment flags (what conversion will
// carry onto the order lines) with per-line `qty_to_invoice = quantity` and status `no` — there
// are no watermarks before an order exists.
#[tokio::test]
async fn quotation_read_model_has_no_watermarks() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let q = w
        .create_quotation(NewQuotation {
            opportunity_id: None,
            template_id: None,
            quotation_number: uq("QUO"),
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            valid_until: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines: vec![pline(item, "7", InvoicePolicy::Delivery, true)],
        })
        .await
        .unwrap();

    let view = w.quotation_invoice_view(q).await.unwrap();
    assert_eq!(view.invoice_status, "no");
    assert_eq!(view.lines[0].invoice_policy, "delivery");
    assert!(view.lines[0].is_downpayment);
    assert_eq!(n(view.lines[0].qty_to_invoice), d("7"));
    assert_eq!(view.lines[0].invoice_status, "no");
}

// ── IS: the seam mirrors the same basis ────────────────────────────────────────

// IS-1: `build_invoice_request` asks for the POLICY remainder — a delivery-policy line requests
// only its delivered-minus-billed quantity, an order-policy line its ordered-minus-billed.
#[tokio::test]
async fn invoice_request_follows_policy_basis() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, ord_item, del_item) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(
        &w,
        company,
        vec![
            pline(ord_item, "10", InvoicePolicy::Order, false),
            pline(del_item, "10", InvoicePolicy::Delivery, false),
        ],
    )
    .await;
    w.mark_delivered(order, company, &[(del_item, d("4"))]).await.unwrap();

    let req = w.build_invoice_request(order).await.unwrap();
    let get = |item: Uuid| req.lines.iter().find(|l| l.item_id == item).unwrap().quantity;
    assert_eq!(n(get(ord_item)), d("10"), "order policy: full ordered quantity");
    assert_eq!(n(get(del_item)), d("4"), "delivery policy: only the delivered quantity");
}

// IS-2: the billed WATERMARK bound is the same basis — billing a delivery-policy line past its
// delivered quantity is refused (`over_billed`), so revenue can never run ahead of delivery.
#[tokio::test]
async fn watermark_bound_is_policy_basis() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Delivery, false)]).await;
    w.mark_delivered(order, company, &[(item, d("4"))]).await.unwrap();

    assert!(matches!(
        w.mark_invoiced(order, company, &[(item, d("5"))]).await.unwrap_err(),
        SellingError::OverBilled
    ));
    w.mark_invoiced(order, company, &[(item, d("4"))]).await.unwrap();
    let bq: Decimal = sqlx::query_scalar(
        "SELECT billed_qty FROM selling.sales_order_items WHERE order_id=$1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bq, d("4.0000"), "a refused over-bill leaves the watermark untouched");
}

// IS-3: after the watermarks move, the read model and the request agree — the single source holds.
#[tokio::test]
async fn read_model_and_request_agree() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Delivery, false)]).await;
    w.mark_delivered(order, company, &[(item, d("7"))]).await.unwrap();
    w.mark_invoiced(order, company, &[(item, d("3"))]).await.unwrap();

    let view = w.order_invoice_view(order).await.unwrap();
    let req = w.build_invoice_request(order).await.unwrap();
    assert_eq!(n(view.lines[0].qty_to_invoice), d("4"));
    assert_eq!(n(req.lines[0].quantity), d("4"));
}

// ── CV: conversion carries the policy + downpayment flags ──────────────────────

// CV-1: quotation → order conversion preserves each line's invoicing policy and downpayment flag
// verbatim — conversion must never change the billing intent the offer committed to.
#[tokio::test]
async fn conversion_carries_policy_and_downpayment() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let q = w
        .create_quotation(NewQuotation {
            opportunity_id: None,
            template_id: None,
            quotation_number: uq("QUO"),
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            valid_until: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines: vec![
                pline(a, "10", InvoicePolicy::Delivery, false),
                pline(b, "1", InvoicePolicy::Order, true),
            ],
        })
        .await
        .unwrap();
    w.accept_quotation(q, company).await.unwrap();
    let order = w.convert_quotation_to_order(q, uq("SO")).await.unwrap();

    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT item_id, invoice_policy::text, is_downpayment FROM selling.sales_order_items WHERE order_id=$1 ORDER BY item_id",
    )
    .bind(order)
    .fetch_all(&pool)
    .await
    .unwrap();
    let expect = |item: Uuid| rows.iter().find(|r| r.0 == item).unwrap();
    assert_eq!(expect(a).1, "delivery");
    assert!(!expect(a).2);
    assert_eq!(expect(b).1, "order");
    assert!(expect(b).2);
}

// ── QT: quotation templates ────────────────────────────────────────────────────

async fn template(w: &SellingWriteService, company: Uuid, name: &str, days: i32, notes: Option<&str>) -> Uuid {
    w.create_quotation_template(company, name, days, notes).await.unwrap()
}

// QT-1: template create + list; a duplicate (company_id, name) refuses loudly.
#[tokio::test]
async fn template_create_list_and_duplicate_refusal() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let name = uq("Standard offer");

    let tid = template(&w, company, &name, 21, Some("Prices exclude VAT.")).await;
    let list = w.list_quotation_templates(company).await.unwrap();
    let t = list.iter().find(|t| t.id == tid).unwrap();
    assert_eq!(t.name, name);
    assert_eq!(t.validity_days, 21);
    assert_eq!(t.default_notes.as_deref(), Some("Prices exclude VAT."));

    let e = w.create_quotation_template(company, &name, 30, None).await.unwrap_err();
    assert!(matches!(e, SellingError::TemplateDuplicate(_)));
    assert_eq!(SellingError::TemplateDuplicate(String::new()).http_status(), 422);

    // Another tenant may hold the same name (the unique index is per company).
    let other = Uuid::new_v4();
    w.create_quotation_template(other, &name, 30, None).await.unwrap();
    assert_eq!(w.list_quotation_templates(other).await.unwrap().len(), 1);
}

// QT-2: a template stamps valid_until (quotation_date + validity_days) and the default notes when
// the caller supplied neither.
#[tokio::test]
async fn template_stamps_validity_and_notes_when_absent() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let tid = template(&w, company, &uq("T"), 15, Some("Auto notes.")).await;

    let q = w
        .create_quotation(NewQuotation {
            opportunity_id: None,
            template_id: Some(tid),
            quotation_number: uq("QUO"),
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
            valid_until: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines: vec![pline(Uuid::new_v4(), "1", InvoicePolicy::Order, false)],
        })
        .await
        .unwrap();
    let (valid_until, notes): (chrono::NaiveDate, Option<String>) =
        sqlx::query_as("SELECT valid_until, notes FROM selling.quotations WHERE id=$1")
            .bind(q)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(valid_until, chrono::NaiveDate::from_ymd_opt(2026, 8, 25).unwrap());
    assert_eq!(notes.as_deref(), Some("Auto notes."));
}

// QT-3: the caller's own values always win — a supplied valid_until/notes is not overwritten.
#[tokio::test]
async fn caller_values_beat_the_template() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let tid = template(&w, company, &uq("T"), 15, Some("Auto notes.")).await;

    let q = w
        .create_quotation(NewQuotation {
            opportunity_id: None,
            template_id: Some(tid),
            quotation_number: uq("QUO"),
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
            valid_until: Some(chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()),
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: Some("Hand written.".into()),
            lines: vec![pline(Uuid::new_v4(), "1", InvoicePolicy::Order, false)],
        })
        .await
        .unwrap();
    let (valid_until, notes): (chrono::NaiveDate, Option<String>) =
        sqlx::query_as("SELECT valid_until, notes FROM selling.quotations WHERE id=$1")
            .bind(q)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(valid_until, chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap());
    assert_eq!(notes.as_deref(), Some("Hand written."));
}

// QT-4: an unknown (or other-tenant) template id refuses with `template_not_found`.
#[tokio::test]
async fn unknown_template_refuses() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let stranger = template(&w, Uuid::new_v4(), &uq("T"), 15, None).await; // another tenant's

    for tid in [Uuid::new_v4(), stranger] {
        let e = w
            .create_quotation(NewQuotation {
                opportunity_id: None,
                template_id: Some(tid),
                quotation_number: uq("QUO"),
                company_id: company,
                branch_id: None,
                customer_id: Uuid::new_v4(),
                quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
                valid_until: None,
                currency: None,
                tax_rate: Decimal::ZERO,
                notes: None,
                lines: vec![pline(Uuid::new_v4(), "1", InvoicePolicy::Order, false)],
            })
            .await
            .unwrap_err();
        assert!(matches!(e, SellingError::TemplateNotFound(_)));
        assert_eq!(SellingError::TemplateNotFound(Uuid::new_v4()).http_status(), 422);
    }
}

// ── OP: the opportunity link ───────────────────────────────────────────────────

// OP-1: the host-passed opportunity id persists on the quotation (a logical link — selling takes
// it on faith; no cross-module key).
#[tokio::test]
async fn opportunity_id_persists() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, opportunity) = (Uuid::new_v4(), Uuid::new_v4());
    let q = w
        .create_quotation(NewQuotation {
            opportunity_id: Some(opportunity),
            template_id: None,
            quotation_number: uq("QUO"),
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            quotation_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            valid_until: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines: vec![pline(Uuid::new_v4(), "1", InvoicePolicy::Order, false)],
        })
        .await
        .unwrap();
    let stored: Option<Uuid> = sqlx::query_scalar("SELECT opportunity_id FROM selling.quotations WHERE id=$1")
        .bind(q)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, Some(opportunity));
}

// ── LF: the order-line freeze ──────────────────────────────────────────────────

// LF-1: once the order's status has left draft, item/qty/price/discount edits refuse with
// `order_line_frozen`; the description stays editable at every status.
#[tokio::test]
async fn order_line_freeze() {
    let pool = pool().await;
    let rec = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(rec.clone()));
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Order, false)]).await;
    let lid = line_id(&pool, order, item).await;

    // Confirmed: description-only edit is allowed.
    w.update_order_line(lid, company, UpdateOrderLinePatch {
        description: Some("renamed label".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let desc: String = sqlx::query_scalar("SELECT description FROM selling.sales_order_items WHERE id=$1")
        .bind(lid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(desc, "renamed label");

    // Confirmed: every frozen field refuses.
    for patch in [
        UpdateOrderLinePatch { quantity: Some(d("5")), ..Default::default() },
        UpdateOrderLinePatch { unit_price: Some(d("1")), ..Default::default() },
        UpdateOrderLinePatch { line_discount: Some(d("1")), ..Default::default() },
        UpdateOrderLinePatch { item_id: Some(Uuid::new_v4()), ..Default::default() },
    ] {
        let e = w.update_order_line(lid, company, patch).await.unwrap_err();
        assert!(matches!(e, SellingError::OrderLineFrozen));
        assert_eq!(SellingError::OrderLineFrozen.http_status(), 422);
    }
    let qty: Decimal = sqlx::query_scalar("SELECT quantity FROM selling.sales_order_items WHERE id=$1")
        .bind(lid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(qty, d("10.0000"), "a refused edit leaves the line untouched");
}

// LF-2: on a DRAFT order a priced edit re-prices the line AND re-derives the header totals in the
// same transaction (2dp half-up money convention).
#[tokio::test]
async fn draft_line_edit_reprices_totals() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = w
        .create_sales_order(NewSalesOrder {
            order_number: uq("SO"),
            quotation_id: None,
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            delivery_date: None,
            currency: None,
            tax_rate: d("11"),
            notes: None,
            lines: vec![NewLine {
                invoice_policy: None,
                is_downpayment: None,
                item_id: item,
                revenue_account_id: None,
                description: None,
                quantity: d("10"),
                unit_price: d("100000"),
                line_discount: Decimal::ZERO,
            }],
        })
        .await
        .unwrap();
    let lid = line_id(&pool, order, item).await;

    w.update_order_line(lid, company, UpdateOrderLinePatch {
        quantity: Some(d("3")),
        unit_price: Some(d("50000")),
        line_discount: Some(d("10000")),
        ..Default::default()
    })
    .await
    .unwrap();
    let (amount, subtotal, tax, total): (Decimal, Decimal, Decimal, Decimal) = sqlx::query_as(
        "SELECT soi.line_amount, o.subtotal, o.tax_amount, o.total \
         FROM selling.sales_order_items soi JOIN selling.sales_orders o ON o.id = soi.order_id \
         WHERE soi.id=$1",
    )
    .bind(lid)
    .fetch_one(&pool)
    .await
    .unwrap();
    // line: 3 × 50,000 − 10,000 = 140,000; header: tax 11% on 140,000.
    assert_eq!(amount, d("140000.00"));
    assert_eq!(subtotal, d("140000.00"));
    assert_eq!(tax, d("15400.00"));
    assert_eq!(total, d("155400.00"));
}

// ── OM: the order cancel guard ─────────────────────────────────────────────────

// OM-1: cancelling a draft order flips it to cancelled and emits SalesOrderCancelled.
#[tokio::test]
async fn cancel_draft_order() {
    let pool = pool().await;
    let rec = RecordingSink::default();
    let w = SellingWriteService::with_sink(pool.clone(), Arc::new(rec.clone()));
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = w
        .create_sales_order(NewSalesOrder {
            order_number: uq("SO"),
            quotation_id: None,
            company_id: company,
            branch_id: None,
            customer_id: Uuid::new_v4(),
            order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            delivery_date: None,
            currency: None,
            tax_rate: Decimal::ZERO,
            notes: None,
            lines: vec![pline(item, "10", InvoicePolicy::Order, false)],
        })
        .await
        .unwrap();

    w.cancel_sales_order(order, company).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "cancelled");
    assert!(rec.has(|e| matches!(e, SellingEvent::SalesOrderCancelled(e) if e.order_id == order)));
}

// OM-2: a billed line refuses the cancel — posted invoices are never cancelled (`order_billed`);
// a delivered-but-unbilled order CAN still be cancelled (delivery reversal is inventory's lane).
#[tokio::test]
async fn cancel_refuses_billed_allows_delivered() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let billed = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Order, false)]).await;
    w.mark_invoiced(billed, company, &[(item, d("1"))]).await.unwrap();

    let e = w.cancel_sales_order(billed, company).await.unwrap_err();
    assert!(matches!(e, SellingError::OrderBilled));
    assert_eq!(SellingError::OrderBilled.http_status(), 422);
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(billed)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "to_deliver_and_bill", "a refused cancel leaves the state untouched");

    let delivered = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Order, false)]).await;
    w.mark_delivered(delivered, company, &[(item, d("10"))]).await.unwrap();
    w.cancel_sales_order(delivered, company).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(delivered)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "cancelled");
}

// OM-3: a terminal order (completed) refuses the cancel with `invalid_transition`.
#[tokio::test]
async fn cancel_refuses_completed() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let (company, item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = confirmed_order(&w, company, vec![pline(item, "10", InvoicePolicy::Order, false)]).await;
    w.mark_delivered(order, company, &[(item, d("10"))]).await.unwrap();
    w.mark_invoiced(order, company, &[(item, d("10"))]).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(st, "completed");

    let e = w.cancel_sales_order(order, company).await.unwrap_err();
    assert!(matches!(e, SellingError::InvalidTransition { ref verb, ref current }
        if verb == "cancel" && current == "completed"));
}

// OM-4: an unknown or wrong-tenant order id is a 404, not a guard refusal.
#[tokio::test]
async fn cancel_unknown_order_is_not_found() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let foreign = confirmed_order(&w, Uuid::new_v4(), vec![pline(Uuid::new_v4(), "1", InvoicePolicy::Order, false)]).await;

    for oid in [Uuid::new_v4(), foreign] {
        assert!(matches!(w.cancel_sales_order(oid, company).await.unwrap_err(), SellingError::OrderNotFound(_)));
    }
}
