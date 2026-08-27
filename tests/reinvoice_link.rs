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
async fn draft_order(w: &SellingWriteService, company: Uuid) -> Uuid {
    w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, delivery_carrier_id: None,
        company_id: company, branch_id: None, customer_id: Uuid::new_v4(),
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
    let company = Uuid::new_v4();
    let order = draft_order(&w, company).await;
    let expense = Uuid::new_v4(); // on faith — an arbitrary id is exactly the contract

    let link = w.attach_expense_reinvoice(order, expense, d("150000.00"), company).await.unwrap();
    let (amount, state): (Decimal, String) = sqlx::query(
        "SELECT amount, state::text FROM selling.expense_reinvoice_links WHERE id=$1")
        .bind(link).fetch_one(&pool).await
        .map(|r| (r.get("amount"), r.get("state"))).unwrap();
    assert_eq!(amount, d("150000.00"));
    assert_eq!(state, "pending");

    assert!(matches!(
        w.attach_expense_reinvoice(order, Uuid::new_v4(), d("0"), company).await.unwrap_err(),
        SellingError::InvalidReinvoiceAmount
    ));
    assert!(matches!(
        w.attach_expense_reinvoice(order, Uuid::new_v4(), d("-1"), company).await.unwrap_err(),
        SellingError::InvalidReinvoiceAmount
    ));
}

// R2: the same expense cannot be attached twice to one order (the double-bill guard — pre-read
// first, the partial unique index backs the race). The SAME expense on a DIFFERENT order is fine.
#[tokio::test]
async fn duplicate_order_expense_pair_refuses() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let order = draft_order(&w, company).await;
    let expense = Uuid::new_v4();

    w.attach_expense_reinvoice(order, expense, d("10.00"), company).await.unwrap();
    assert!(matches!(
        w.attach_expense_reinvoice(order, expense, d("20.00"), company).await.unwrap_err(),
        SellingError::DuplicateReinvoice
    ));
    // a different expense on the same order, or the same expense on another order: both fine.
    w.attach_expense_reinvoice(order, Uuid::new_v4(), d("5.00"), company).await.unwrap();
    let other = draft_order(&w, company).await;
    w.attach_expense_reinvoice(other, expense, d("10.00"), company).await.unwrap();
}

// R3: attach refuses on a CANCELLED order (`invalid_transition` — nothing may rebill against a
// dead order) and on an unknown/wrong-tenant order (`order_not_found`, no leak).
#[tokio::test]
async fn attach_refuses_cancelled_and_unknown_orders() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let cancelled = draft_order(&w, company).await;
    w.cancel_sales_order(cancelled, company, &NoStockFulfillmentPort).await.unwrap();

    match w.attach_expense_reinvoice(cancelled, Uuid::new_v4(), d("10.00"), company).await.unwrap_err() {
        SellingError::InvalidTransition { verb, current } => {
            assert_eq!(verb, "attach_expense_reinvoice");
            assert_eq!(current, "cancelled");
        }
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    // unknown order and cross-tenant order are both plain not-found.
    assert!(matches!(
        w.attach_expense_reinvoice(Uuid::new_v4(), Uuid::new_v4(), d("10.00"), company).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
    let foreign_order = draft_order(&w, Uuid::new_v4()).await;
    assert!(matches!(
        w.attach_expense_reinvoice(foreign_order, Uuid::new_v4(), d("10.00"), company).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
}

// R4: mark-invoiced flips pending → invoiced; a DOUBLE mark is a LOUD refusal (a billing retry
// must surface, not silently pass); an unknown link is 404-shaped.
#[tokio::test]
async fn mark_invoiced_flips_once_and_refuses_loudly_on_repeat() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let order = draft_order(&w, company).await;
    let link = w.attach_expense_reinvoice(order, Uuid::new_v4(), d("75.50"), company).await.unwrap();

    w.mark_expense_reinvoice_invoiced(link, company).await.unwrap();
    let state: String = sqlx::query_scalar("SELECT state::text FROM selling.expense_reinvoice_links WHERE id=$1")
        .bind(link).fetch_one(&pool).await.unwrap();
    assert_eq!(state, "invoiced");

    match w.mark_expense_reinvoice_invoiced(link, company).await.unwrap_err() {
        SellingError::InvalidTransition { verb, current } => {
            assert_eq!(verb, "mark_invoiced");
            assert_eq!(current, "invoiced");
        }
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    assert!(matches!(
        w.mark_expense_reinvoice_invoiced(Uuid::new_v4(), company).await.unwrap_err(),
        SellingError::ReinvoiceNotFound(_)
    ));
    // cross-tenant mark is indistinguishable from unknown.
    assert!(matches!(
        w.mark_expense_reinvoice_invoiced(link, Uuid::new_v4()).await.unwrap_err(),
        SellingError::ReinvoiceNotFound(_)
    ));
}

// R5: the list returns the order's links (the billing adapter's pull read); an unknown order
// refuses, and another company's order id yields no listings.
#[tokio::test]
async fn list_serves_an_orders_links_and_fences_unknown_orders() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let order = draft_order(&w, company).await;
    let e1 = Uuid::new_v4();
    let e2 = Uuid::new_v4();
    let first = w.attach_expense_reinvoice(order, e1, d("10.00"), company).await.unwrap();
    let second = w.attach_expense_reinvoice(order, e2, d("20.00"), company).await.unwrap();
    w.mark_expense_reinvoice_invoiced(first, company).await.unwrap();

    let links = w.list_expense_reinvoices(order, company).await.unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(links.iter().find(|l| l.id == first).unwrap().state, "invoiced");
    assert_eq!(links.iter().find(|l| l.id == second).unwrap().state, "pending");
    assert_eq!(links.iter().find(|l| l.id == second).unwrap().amount, d("20.00"));

    assert!(matches!(
        w.list_expense_reinvoices(Uuid::new_v4(), company).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
    // the explicit company filter fences even without RLS binding (superuser pool).
    let foreign = draft_order(&w, Uuid::new_v4()).await;
    assert!(matches!(
        w.list_expense_reinvoices(foreign, company).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
}

// ── route-level probe ────────────────────────────────────────────────────────

const SECRET: &[u8] = b"selling-reinvoice-probe-secret";

#[derive(serde::Serialize)]
struct TestClaims {
    sub: String,
    exp: usize,
    company_id: Uuid,
}
fn token(company_id: Uuid) -> String {
    let claims = TestClaims { sub: "reinvoice-probe".into(), exp: 9_999_999_999, company_id };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(SECRET),
    ).unwrap()
}

async fn probe_app() -> axum::Router {
    let pool = pool().await;
    let m = backbone_selling::SellingModule::builder().with_database(pool.clone()).build().unwrap();
    backbone_selling::presentation::http::create_guarded_selling_routes(
        &m,
        pool,
        backbone_auth::company::CompanyVerifier::hs256(SECRET),
        std::sync::Arc::new(NoUnitCostPort),
        std::sync::Arc::new(NoStockFulfillmentPort),
    )
}

async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
    company: Option<Uuid>,
) -> (axum::http::StatusCode, String) {
    use tower::ServiceExt;
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(c) = company {
        builder = builder.header("authorization", format!("Bearer {}", token(c)));
    }
    let resp = app
        .oneshot(builder.body(axum::body::Body::from(body.unwrap_or_default())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// The three verbs served through the guarded routes, in the billing adapter's order: attach →
// list (pending) → mark-invoiced; every step demands a token; a foreign tenant sees nothing.
#[tokio::test]
async fn reinvoice_routes_serve_the_billing_adapter_pull() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let order = draft_order(&w, company).await;
    let expense = Uuid::new_v4();
    let app = probe_app().await;

    // token demanded on every verb.
    for (method, uri, body) in [
        ("POST", format!("/sales-orders/{order}/expense-reinvoices"), Some(r#"{"expenseId":"00000000-0000-0000-0000-000000000000","amount":"1"}"#.into())),
        ("GET", format!("/sales-orders/{order}/expense-reinvoices"), None),
        ("POST", "/expense-reinvoices/00000000-0000-0000-0000-000000000000/mark-invoiced".into(), None),
    ] {
        let (status, _) = send(app.clone(), method, &uri, body, None).await;
        assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED, "{method} {uri} must demand a token");
    }

    // attach through the route.
    let (status, created) = send(
        app.clone(), "POST", &format!("/sales-orders/{order}/expense-reinvoices"),
        Some(format!(r#"{{"expenseId":"{expense}","amount":"250000.00"}}"#)), Some(company),
    ).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let link: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();

    // duplicate through the route refuses 422.
    let (status, body) = send(
        app.clone(), "POST", &format!("/sales-orders/{order}/expense-reinvoices"),
        Some(format!(r#"{{"expenseId":"{expense}","amount":"1.00"}}"#)), Some(company),
    ).await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("duplicate_reinvoice"));

    // the pull list serves camelCase rows.
    let (status, body) = send(app.clone(), "GET", &format!("/sales-orders/{order}/expense-reinvoices"), None, Some(company)).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mine = list.as_array().unwrap().iter().find(|l| l["id"] == link.to_string()).expect("listed");
    assert_eq!(mine["state"], "pending");
    assert_eq!(mine["expenseId"], expense.to_string());

    // a foreign tenant neither lists nor marks the victim's link.
    let (status, _) = send(
        app.clone(), "GET", &format!("/sales-orders/{order}/expense-reinvoices"), None, Some(Uuid::new_v4()),
    ).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "a foreign tenant must not see the order's links");
    let (status, _) = send(
        app.clone(), "POST", &format!("/expense-reinvoices/{link}/mark-invoiced"), None, Some(Uuid::new_v4()),
    ).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "a foreign tenant must not mark the link");

    // mark through the route; the double mark is loud.
    let (status, body) = send(
        app.clone(), "POST", &format!("/expense-reinvoices/{link}/mark-invoiced"), None, Some(company),
    ).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let (status, body) = send(
        app.clone(), "POST", &format!("/expense-reinvoices/{link}/mark-invoiced"), None, Some(company),
    ).await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY, "double mark must be loud: {body}");
    assert!(body.contains("invalid_transition"));
}
