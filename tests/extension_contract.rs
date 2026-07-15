//! Extension-contract §5, SECOND clause: a CONSUMER adds a custom rule via an event subscription,
//! and that rule survives a regeneration of both modules.
//!
//! This test drives the runtime half: selling emits `SalesOrderConfirmed`; the reference consumer
//! (`CreditWatchConsumer`, a user-owned `*_custom.rs`) subscribes and applies its OWN rule (credit
//! limit) without selling knowing anything about it. The regen-survival half is proven by
//! `scripts/regen_roundtrip.sh` (runs `metaphor schema schema generate --force` on both modules and
//! asserts the user-owned files + this test are untouched and still green).
//! Requires DATABASE_URL (:5433/backbone_selling).

use std::sync::Arc;

use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::consumer_credit_rule_custom::CreditWatchConsumer;
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesOrder, SellingWriteService,
};

fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }
fn day(y: i32, m: u32, dd: u32) -> chrono::NaiveDate { chrono::NaiveDate::from_ymd_opt(y, m, dd).unwrap() }
fn uq(p: &str) -> String { format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8]) }
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect DB")
}

async fn make_order(w: &SellingWriteService, company: Uuid, customer: Uuid, total_price: &str) -> Uuid {
    w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, company_id: company, branch_id: None,
        customer_id: customer, order_date: day(2026, 7, 3), delivery_date: None, currency: None,
        tax_rate: Decimal::ZERO, notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: None, description: None,
            quantity: d("1"), unit_price: d(total_price), line_discount: Decimal::ZERO }],
    }).await.unwrap()
}

// EXT-1: a consumer rule fires on the selling domain event — an over-limit order is flagged, an
// under-limit order is not. Selling emits; the consumer decides. No coupling back into selling.
#[tokio::test]
async fn consumer_rule_rides_domain_event() {
    let pool = pool().await;
    let (company, customer) = (Uuid::new_v4(), Uuid::new_v4());

    // The consumer wires its rule as the event sink (a real deployment wires a bus adapter).
    let consumer = Arc::new(CreditWatchConsumer::new(d("5000000"))); // 5,000,000 credit limit
    let breaches = consumer.breaches();
    let w = SellingWriteService::with_sink(pool.clone(), consumer);

    // Under the limit → confirmed, no breach recorded.
    let ok_order = make_order(&w, company, customer, "1000000").await;
    w.confirm_sales_order(ok_order, company).await.unwrap();
    assert_eq!(breaches.lock().unwrap().len(), 0, "under-limit order does not breach");

    // Over the limit → confirmed, consumer records a breach (its own concept, not selling's).
    let big_order = make_order(&w, company, customer, "9000000").await;
    w.confirm_sales_order(big_order, company).await.unwrap();
    let recorded = breaches.lock().unwrap();
    assert_eq!(recorded.len(), 1, "over-limit order breaches");
    assert_eq!(recorded[0].order_id, big_order);
    assert_eq!(recorded[0].grand_total, d("9000000"));
    assert_eq!(recorded[0].limit, d("5000000"));
}

// EXT-2: the default (no consumer) path still confirms orders — the event surface is additive,
// selling does not depend on any subscriber existing.
#[tokio::test]
async fn selling_works_without_any_consumer() {
    let pool = pool().await;
    let (company, customer) = (Uuid::new_v4(), Uuid::new_v4());
    let w = SellingWriteService::new(pool.clone()); // default LoggingSink, no consumer
    let order = make_order(&w, company, customer, "9000000").await;
    w.confirm_sales_order(order, company).await.unwrap();
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "to_deliver_and_bill");
}
