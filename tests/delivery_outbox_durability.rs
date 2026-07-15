//! Durability probe (outbox rollout plan, P1): the cross-module `DeliveryRequested` event — which inventory
//! SUBSCRIBES to, to move stock + post COGS — is staged in the transactional outbox, so a crash between the
//! request and the in-proc publish cannot drop it. The default `LoggingSink` drops the in-proc publish; the
//! event must still be durably staged in `selling.outbox_events` for the relay to drain.

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_write_service::{NewLine, NewSalesOrder, SellingWriteService};

fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }
fn day() -> chrono::NaiveDate { chrono::NaiveDate::from_ymd_opt(2026, 7, 4).unwrap() }
fn uq(p: &str) -> String { format!("{p}-{}", &Uuid::new_v4().simple().to_string()[..8]) }
async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.expect("connect")
}

// DOD-1 — a confirmed order's delivery request durably stages DeliveryRequested despite the dropped publish.
#[tokio::test]
async fn dod1_delivery_request_is_durably_staged() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let selling = SellingWriteService::new(pool.clone()); // default LoggingSink → in-proc publish is dropped

    let oid = selling.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, company_id: company, branch_id: None,
        customer_id: Uuid::new_v4(), order_date: day(), delivery_date: None, currency: None,
        tax_rate: d("0"), notes: None,
        lines: vec![NewLine { item_id: Uuid::new_v4(), revenue_account_id: None, description: None,
            quantity: d("10"), unit_price: d("150000"), line_discount: Decimal::ZERO }],
    }).await.unwrap();
    selling.confirm_sales_order(oid, company).await.unwrap();

    selling.build_delivery_request(oid).await.unwrap();

    let staged: i64 = sqlx::query(
        "SELECT count(*) AS n FROM selling.outbox_events WHERE aggregate_id=$1 AND event_type='DeliveryRequested'")
        .bind(oid.to_string()).fetch_one(&pool).await.unwrap().get("n");
    assert_eq!(staged, 1, "DeliveryRequested durably staged despite the dropped in-proc publish");
}
