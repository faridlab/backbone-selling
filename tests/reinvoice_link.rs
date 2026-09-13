//! The expense-reinvoice link — selling's side of the rebill-expenses-to-the-customer seam.
//!
//! Requires DATABASE_URL (:5433/backbone_selling). Service-level cases pin the three verbs the
//! host billing adapter drives (attach / list / mark-invoiced) and the double-bill guards; the
//! route probe serves the pull surface end-to-end.
//!
//! `expense_id` is taken ON FAITH (no cross-module key): these tests use arbitrary expense ids on
//! purpose — selling must not care whether backbone-expenses knows them.
//!
//! Coverage map:
//!   attach happy path (pending state) + draft-order attach is allowed
//!   non-positive amounts refuse `invalid_reinvoice_amount`
//!   duplicate (order, expense) refuses `duplicate_reinvoice` (the partial unique index's pre-read)
//!   attach to cancelled order refuses `invalid_transition`; unknown order ⇒ 404-shaped refusal
//!   mark-invoiced happy path; a double mark is a LOUD refusal; unknown link ⇒ 404-shaped refusal
//!   list per order (created order) and its route serving

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_stock_fulfillment::NoStockFulfillmentPort;
use backbone_selling::application::service::selling_service_catalog::NoServiceCatalog;
use backbone_selling::application::service::selling_service_delivery::NoServiceDelivery;
use backbone_selling::application::service::selling_unit_cost::NoUnitCostPort;
use backbone_selling::application::service::selling_write_service::{
    NewLine, NewSalesOrder, SellingError, SellingWriteService,
};

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
fn line() -> NewLine {
    NewLine { invoice_policy: None, is_downpayment: None,
        item_id: Uuid::new_v4(), revenue_account_id: None, description: None,
        quantity: d("1"), unit_price: d("1000"), line_discount: d("0"),
    }
}
async fn draft_order(w: &SellingWriteService) -> Uuid {
    w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, delivery_carrier_id: None, branch_id: None, customer_id: Uuid::new_v4(),
        order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 25).unwrap(),
        delivery_date: None, currency: None, tax_rate: d("0"), notes: None,
        lines: vec![line()],
    }).await.unwrap()
}

// R1: attach stores a pending link with the exact amount; attaching to a DRAFT order is allowed
// (estimating a quote-era charge before confirm is normal); zero and negative amounts refuse.
#[tokio::test]
async fn attach_creates_a_pending_link_and_guards_the_amount() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let order = draft_order(&w).await;
    let expense = Uuid::new_v4(); // on faith — an arbitrary id is exactly the contract

    let link = w.attach_expense_reinvoice(order, expense, d("150000.00")).await.unwrap();
    let (amount, state): (Decimal, String) = sqlx::query(
        "SELECT amount, state::text FROM selling.expense_reinvoice_links WHERE id=$1")
        .bind(link).fetch_one(&pool).await
        .map(|r| (r.get("amount"), r.get("state"))).unwrap();
    assert_eq!(amount, d("150000.00"));
    assert_eq!(state, "pending");

    assert!(matches!(
        w.attach_expense_reinvoice(order, Uuid::new_v4(), d("0")).await.unwrap_err(),
        SellingError::InvalidReinvoiceAmount
    ));
    assert!(matches!(
        w.attach_expense_reinvoice(order, Uuid::new_v4(), d("-1")).await.unwrap_err(),
        SellingError::InvalidReinvoiceAmount
    ));
}

// R2: the same expense cannot be attached twice to one order (the double-bill guard — pre-read
// first, the partial unique index backs the race). The SAME expense on a DIFFERENT order is fine.
#[tokio::test]
async fn duplicate_order_expense_pair_refuses() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let order = draft_order(&w).await;
    let expense = Uuid::new_v4();

    w.attach_expense_reinvoice(order, expense, d("10.00")).await.unwrap();
    assert!(matches!(
        w.attach_expense_reinvoice(order, expense, d("20.00")).await.unwrap_err(),
        SellingError::DuplicateReinvoice
    ));
    // a different expense on the same order, or the same expense on another order: both fine.
    w.attach_expense_reinvoice(order, Uuid::new_v4(), d("5.00")).await.unwrap();
    let other = draft_order(&w).await;
    w.attach_expense_reinvoice(other, expense, d("10.00")).await.unwrap();
}

// R3: attach refuses on a CANCELLED order (`invalid_transition` — nothing may rebill against a
// dead order) and on an unknown order (`order_not_found`, no leak).
#[tokio::test]
async fn attach_refuses_cancelled_and_unknown_orders() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let cancelled = draft_order(&w).await;
    w.cancel_sales_order(cancelled, &NoStockFulfillmentPort).await.unwrap();

    match w.attach_expense_reinvoice(cancelled, Uuid::new_v4(), d("10.00")).await.unwrap_err() {
        SellingError::InvalidTransition { verb, current } => {
            assert_eq!(verb, "attach_expense_reinvoice");
            assert_eq!(current, "cancelled");
        }
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    // an unknown order is plain not-found (under a composed tenancy decorator a foreign id is
    // indistinguishable — which is the point, ADR-0029).
    assert!(matches!(
        w.attach_expense_reinvoice(Uuid::new_v4(), Uuid::new_v4(), d("10.00")).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
}

// R4: mark-invoiced flips pending → invoiced; a DOUBLE mark is a LOUD refusal (a billing retry
// must surface, not silently pass); an unknown link is 404-shaped.
#[tokio::test]
async fn mark_invoiced_flips_once_and_refuses_loudly_on_repeat() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let order = draft_order(&w).await;
    let link = w.attach_expense_reinvoice(order, Uuid::new_v4(), d("75.50")).await.unwrap();

    w.mark_expense_reinvoice_invoiced(link).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state::text FROM selling.expense_reinvoice_links WHERE id=$1")
        .bind(link).fetch_one(&pool).await.unwrap();
    assert_eq!(state, "invoiced");

    match w.mark_expense_reinvoice_invoiced(link).await.unwrap_err() {
        SellingError::InvalidTransition { verb, current } => {
            assert_eq!(verb, "mark_invoiced");
            assert_eq!(current, "invoiced");
        }
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    assert!(matches!(
        w.mark_expense_reinvoice_invoiced(Uuid::new_v4()).await.unwrap_err(),
        SellingError::ReinvoiceNotFound(_)
    ));
}

// R5: the list returns the order's links (the billing adapter's pull read); an unknown order
// id refuses.
#[tokio::test]
async fn list_serves_an_orders_links_and_refuses_unknown_orders() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let order = draft_order(&w).await;
    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let first = w.attach_expense_reinvoice(order, e1, d("10.00")).await.unwrap();
    let second = w.attach_expense_reinvoice(order, e2, d("20.00")).await.unwrap();
    w.mark_expense_reinvoice_invoiced(first).await.unwrap();

    let links = w.list_expense_reinvoices(order).await.unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(links.iter().find(|l| l.id == first).unwrap().state, "invoiced");
    assert_eq!(links.iter().find(|l| l.id == second).unwrap().state, "pending");
    assert_eq!(links.iter().find(|l| l.id == second).unwrap().amount, d("20.00"));

    assert!(matches!(
        w.list_expense_reinvoices(Uuid::new_v4()).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
    // Row isolation between orders is the composing tenancy decorator's concern (ADR-0029) —
    // pinned, for the module's own posture, in tests/tenancy_posture_probe.rs.
}

// ── route-level probe ────────────────────────────────────────────────────────

async fn probe_app() -> axum::Router {
    let pool = pool().await;
    let m = backbone_selling::SellingModule::builder().with_database(pool.clone()).build().unwrap();
    backbone_selling::presentation::http::create_guarded_selling_routes(
        &m,
        pool,
        std::sync::Arc::new(NoUnitCostPort),
        std::sync::Arc::new(NoStockFulfillmentPort),
        std::sync::Arc::new(NoServiceCatalog),
        std::sync::Arc::new(NoServiceDelivery),
    )
    // The write handlers extract OrgContext, which the composing service's org-session
    // guard inserts in production; the probe app stands the context in directly.
    .layer(axum::middleware::from_fn(
        |mut req: axum::http::Request<axum::body::Body>, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(backbone_auth::org::OrgContext {
                acting_unit_id: uuid::Uuid::nil(),
                entitled_units: vec![],
                legacy_company_id: None,
                user_id: "probe".to_string(),
            });
            next.run(req).await
        },
    ))
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (axum::http::StatusCode, String) {
    use tower::ServiceExt;
    let builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let resp = app
        .oneshot(builder.body(axum::body::Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// The three verbs served through the module surface, in the billing adapter's order: attach →
// list (pending) → mark-invoiced. Token-demand is not probed here: the module applies no auth of
// its own (an internal guard would shadow the composer's fence variables), so the composing
// service's org-session guard owns that property.
#[tokio::test]
async fn reinvoice_routes_serve_the_billing_adapter_pull() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let order = draft_order(&w).await;
    let expense = Uuid::new_v4();
    let app = probe_app().await;

    // attach through the route.
    let (status, created) = send(
        app.clone(), "POST", &format!("/sales-orders/{order}/expense-reinvoices"),
        Some(format!(r#"{{"expenseId":"{expense}","amount":"250000.00"}}"#)),
    ).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let link: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();

    // duplicate through the route refuses 422.
    let (status, body) = send(
        app.clone(), "POST", &format!("/sales-orders/{order}/expense-reinvoices"),
        Some(format!(r#"{{"expenseId":"{expense}","amount":"1.00"}}"#)),
    ).await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("duplicate_reinvoice"));

    // the pull list serves camelCase rows.
    let (status, body) = send(app.clone(), "GET", &format!("/sales-orders/{order}/expense-reinvoices"), None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mine = list.as_array().unwrap().iter().find(|l| l["id"] == link.to_string()).expect("listed");
    assert_eq!(mine["state"], "pending");
    assert_eq!(mine["expenseId"], expense.to_string());

    // Row isolation between callers is the composing tenancy decorator's concern (ADR-0029);
    // the module's own posture (armed flags, zero policies) is pinned in
    // tests/tenancy_posture_probe.rs.

    // mark through the route; the double mark is loud.
    let (status, body) = send(
        app.clone(), "POST", &format!("/expense-reinvoices/{link}/mark-invoiced"), None,
    ).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let (status, body) = send(
        app.clone(), "POST", &format!("/expense-reinvoices/{link}/mark-invoiced"), None,
    ).await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY, "double mark must be loud: {body}");
    assert!(body.contains("invalid_transition"));
}
