//! The unit-cost margin snapshot — the confirm-time stamp, its refusal rules, and the margin read
//! model (mirrors the margin engine contract in `selling_margin.rs`).
//!
//! Requires DATABASE_URL (:5433/backbone_selling). Service-level cases drive confirm through a
//! SCRIPTED cost port (the host's catalog adapter stand-in); the route-level probe proves a client
//! cannot inject a cost or a margin through the HTTP surface.
//!
//! Coverage map:
//!   null-cost confirm proceeds + reads as honest absence (never zero)
//!   port Err / omitted item / negative cost each REFUSE the confirm (order stays draft), verbatim
//!   the stamp is a confirm-time snapshot — a losing confirm's stamp rolls back with the guard
//!   line/order margin math incl. negatives and zero-amount lines
//!   mixed costed/uncosted coverage (rollup over the costed subset only)
//!   route probe: injected `unitCost`/`margin` fields are ignored; margin route is token-guarded

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_selling::application::service::selling_margin::{line_margin, margin_percent};
use backbone_selling::application::service::selling_stock_fulfillment::NoStockFulfillmentPort;
use backbone_selling::application::service::selling_service_catalog::NoServiceCatalog;
use backbone_selling::application::service::selling_service_delivery::NoServiceDelivery;
use backbone_selling::application::service::selling_unit_cost::{
    ItemUnitCost, NoUnitCostPort, UnitCostError, UnitCostPort, UnitCostRequest,
};
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
fn line(item: Uuid, qty: &str, price: &str, discount: &str) -> NewLine {
    NewLine { invoice_policy: None, is_downpayment: None,
        item_id: item,
        revenue_account_id: None,
        description: None,
        quantity: d(qty),
        unit_price: d(price),
        line_discount: d(discount),
    }
}
async fn draft_order(w: &SellingWriteService, company: Uuid, lines: Vec<NewLine>) -> Uuid {
    w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, delivery_carrier_id: None,
        company_id: company, branch_id: None, customer_id: Uuid::new_v4(),
        order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 25).unwrap(),
        delivery_date: None, currency: None, tax_rate: d("0"), notes: None, lines,
    }).await.unwrap()
}
async fn status(pool: &PgPool, order: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order).fetch_one(pool).await.unwrap()
}
async fn line_costs(pool: &PgPool, order: Uuid) -> Vec<Option<Decimal>> {
    // Timestamps live in metadata in this module's convention; order is only for determinism.
    sqlx::query_scalar(
        "SELECT unit_cost FROM selling.sales_order_items WHERE order_id=$1 \
         ORDER BY (metadata->>'created_at')::timestamptz, id")
        .bind(order).fetch_all(pool).await.unwrap()
}

/// A scripted catalog standard-cost adapter: fixed costs per item, an optional hard failure, and an
/// optional omission list (items the port silently leaves out of its response).
struct ScriptedCosts {
    costs: HashMap<Uuid, Option<Decimal>>,
    fail_with: Option<UnitCostError>,
    omit: Vec<Uuid>,
}
impl ScriptedCosts {
    fn healthy(costs: impl IntoIterator<Item = (Uuid, Option<&'static str>)>) -> Self {
        ScriptedCosts {
            costs: costs.into_iter().map(|(i, c)| (i, c.map(|s| d(s)))).collect(),
            fail_with: None,
            omit: vec![],
        }
    }
}
#[async_trait]
impl UnitCostPort for ScriptedCosts {
    async fn resolve_unit_costs(&self, req: &UnitCostRequest) -> Result<Vec<ItemUnitCost>, UnitCostError> {
        if let Some(e) = &self.fail_with {
            return Err(e.clone());
        }
        Ok(req
            .item_ids
            .iter()
            .filter(|i| !self.omit.contains(i))
            .map(|i| ItemUnitCost { item_id: *i, unit_cost: self.costs.get(i).copied().flatten() })
            .collect())
    }
}

// The pure computes are the single source the read model and the SQL rollup mirror.
#[test]
fn pure_computes_total_basis_and_zero_amount_guard() {
    // total basis: line_amount − cost·qty, no second rounding step.
    assert_eq!(line_margin(d("1000.00"), d("400.00"), d("2")), d("200.00"));
    // free goods carry their full cost as a negative margin — by design.
    assert_eq!(line_margin(d("0.00"), d("150.00"), d("3")), d("-450.00"));
    // percent is 2dp and refuses to invent one for a zero-amount line.
    assert_eq!(margin_percent(d("250.00"), d("1000.00")), Some(d("25.00")));
    assert_eq!(margin_percent(d("1.00"), d("0.00")), None);
}

// A NULL cost PROCEEDS the confirm (it is honest absence, not a failure) and the margin read
// reports NULL margin / NULL percent / NULL order rollup — never a silent zero.
#[tokio::test]
async fn null_cost_confirms_and_reads_as_absent() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();
    let order = draft_order(&w, company, vec![line(item, "10", "1000", "0")]).await;

    w.confirm_sales_order(order, company, &NoUnitCostPort, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    assert_eq!(status(&pool, order).await, "to_deliver_and_bill");
    assert_eq!(line_costs(&pool, order).await, vec![None], "no cost source ⇒ no snapshot");

    let view = w.order_margin_view(order).await.unwrap();
    assert_eq!(view.margin_lines_total, 1);
    assert_eq!(view.margin_lines_costed, 0, "nothing costed on this order");
    assert_eq!(view.lines[0].unit_cost, None);
    assert_eq!(view.lines[0].margin, None, "absent cost must read as NULL margin");
    assert_eq!(view.lines[0].margin_percent, None);
    assert_eq!(view.order_margin, None, "the order rollup is unknown, not zero");
    assert_eq!(view.order_margin_percent, None);
}

// A failing cost port REFUSES the confirm with the port's error VERBATIM; the order stays draft
// with no snapshot. The refusal is not sticky — a retry against a healthy port confirms.
#[tokio::test]
async fn port_failure_refuses_confirm_verbatim_and_is_not_sticky() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();
    let order = draft_order(&w, company, vec![line(item, "1", "1000", "0")]).await;

    let down = ScriptedCosts {
        costs: HashMap::new(),
        fail_with: Some(UnitCostError { code: "catalog_unavailable".into(), message: "catalog is down".into() }),
        omit: vec![],
    };
    match w.confirm_sales_order(order, company, &down, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap_err() {
        SellingError::CostRejected { code, message } => {
            assert_eq!(code, "catalog_unavailable", "the port's code must ride through verbatim");
            assert_eq!(message, "catalog is down");
        }
        other => panic!("expected CostRejected, got {other:?}"),
    }
    assert_eq!(status(&pool, order).await, "draft", "a refused confirm must leave the order draft");
    assert_eq!(line_costs(&pool, order).await, vec![None], "no snapshot may survive a refusal");

    // Same order, healthy port: the confirm succeeds.
    let up = ScriptedCosts::healthy([(item, Some("500"))]);
    w.confirm_sales_order(order, company, &up, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    assert_eq!(status(&pool, order).await, "to_deliver_and_bill");
    assert_eq!(line_costs(&pool, order).await, vec![Some(d("500"))]);
}

// A port that OMITS a requested item refuses the confirm — an unknown-cost confirm corrupts margin
// analytics silently, so it must not pass.
#[tokio::test]
async fn omitted_item_refuses_confirm() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let order = draft_order(&w, company, vec![line(a, "1", "100", "0"), line(b, "1", "100", "0")]).await;

    let holey = ScriptedCosts { costs: HashMap::new(), fail_with: None, omit: vec![b] };
    match w.confirm_sales_order(order, company, &holey, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap_err() {
        SellingError::CostRejected { code, .. } => assert_eq!(code, "unit_cost_line_missing"),
        other => panic!("expected CostRejected(unit_cost_line_missing), got {other:?}"),
    }
    assert_eq!(status(&pool, order).await, "draft");
}

// A NEGATIVE cost from the port refuses the confirm — costs are @non_negative at the schema layer
// and the port result is held to the same rule.
#[tokio::test]
async fn negative_cost_refuses_confirm() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();
    let order = draft_order(&w, company, vec![line(item, "1", "100", "0")]).await;

    let negative = ScriptedCosts::healthy([(item, Some("-1"))]);
    match w.confirm_sales_order(order, company, &negative, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap_err() {
        SellingError::CostRejected { code, .. } => assert_eq!(code, "unit_cost_negative"),
        other => panic!("expected CostRejected(unit_cost_negative), got {other:?}"),
    }
    assert_eq!(status(&pool, order).await, "draft");
}

// The happy stamp: cost per unit lands on every line, and the read model computes
// margin = line_amount − cost·qty and percent = margin/amount·100 (both 2dp).
#[tokio::test]
async fn confirm_stamps_costs_and_margin_view_computes() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();
    // 10 × 1000 = 10000.00 amount, cost 600 ⇒ margin 10000 − 6000 = 4000.00 (40.00%).
    let order = draft_order(&w, company, vec![line(item, "10", "1000", "0")]).await;

    let costs = ScriptedCosts::healthy([(item, Some("600"))]);
    w.confirm_sales_order(order, company, &costs, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    assert_eq!(line_costs(&pool, order).await, vec![Some(d("600"))]);

    let view = w.order_margin_view(order).await.unwrap();
    assert_eq!(view.margin_lines_costed, 1);
    assert_eq!(view.margin_lines_total, 1);
    assert_eq!(view.lines[0].unit_cost, Some(d("600")));
    assert_eq!(view.lines[0].margin, Some(d("4000.00")));
    assert_eq!(view.lines[0].margin_percent, Some(d("40.00")));
    assert_eq!(view.order_margin, Some(d("4000.00")));
    assert_eq!(view.order_margin_percent, Some(d("40.00")));
}

// The stamp is a CONFIRM-TIME snapshot: a later confirm attempt on the now-confirmed order loses
// the draft guard (NotDraft) and its would-be stamp ROLLS BACK with it — a second cost source can
// never rewrite what the first confirm froze.
#[tokio::test]
async fn a_losing_confirm_rolls_its_stamp_back() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();
    let order = draft_order(&w, company, vec![line(item, "2", "500", "0")]).await;

    let first = ScriptedCosts::healthy([(item, Some("100"))]);
    w.confirm_sales_order(order, company, &first, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();

    let second = ScriptedCosts::healthy([(item, Some("999"))]);
    assert!(matches!(
        w.confirm_sales_order(order, company, &second, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap_err(),
        SellingError::NotDraft(_)
    ));
    assert_eq!(
        line_costs(&pool, order).await,
        vec![Some(d("100"))],
        "the losing confirm's stamp must not survive the refused guard"
    );
}

// Mixed coverage: one costed line, one uncosted line. The rollup covers the COSTED SUBSET ONLY —
// the uncosted line neither contributes to the margin sum nor dilutes the percentage.
#[tokio::test]
async fn rollup_covers_the_costed_subset_only() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let order = draft_order(&w, company, vec![line(a, "5", "1000", "0"), line(b, "1", "777", "0")]).await;

    let costs = ScriptedCosts::healthy([(a, Some("200")), (b, None)]);
    w.confirm_sales_order(order, company, &costs, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();

    let view = w.order_margin_view(order).await.unwrap();
    assert_eq!(view.margin_lines_costed, 1);
    assert_eq!(view.margin_lines_total, 2);
    // costed line: 5000 − 200·5 = 4000.00; percent over the COSTED amount sum (5000), not 5777.
    assert_eq!(view.order_margin, Some(d("4000.00")));
    assert_eq!(view.order_margin_percent, Some(d("80.00")));
    // the uncosted line reads as absent, never zero.
    let uncosted = view.lines.iter().find(|l| l.unit_cost.is_none()).unwrap();
    assert_eq!(uncosted.margin, None);
    assert_eq!(uncosted.margin_percent, None);
}

// Negative margins are REAL and are reported as such: a cost above the price is a loss on the
// line, and a zero-amount costed line (free goods) carries its full cost as a negative margin
// with a NULL percentage (there is no meaningful ratio against a zero amount).
#[tokio::test]
async fn negative_margins_are_reported_not_clamped() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let company = Uuid::new_v4();
    let (loss_item, free_item) = (Uuid::new_v4(), Uuid::new_v4());
    let order = draft_order(
        &w,
        company,
        vec![line(loss_item, "2", "100", "0"), line(free_item, "3", "0", "0")],
    ).await;

    let costs = ScriptedCosts::healthy([(loss_item, Some("150")), (free_item, Some("150"))]);
    w.confirm_sales_order(order, company, &costs, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();

    let view = w.order_margin_view(order).await.unwrap();
    let loss = view.lines.iter().find(|l| l.item_id == loss_item).unwrap();
    assert_eq!(loss.line_amount, d("200.00"));
    assert_eq!(loss.margin, Some(d("-100.00")), "cost > price is a real loss");
    assert_eq!(loss.margin_percent, Some(d("-50.00")));
    let free = view.lines.iter().find(|l| l.item_id == free_item).unwrap();
    assert_eq!(free.line_amount, d("0.00"));
    assert_eq!(free.margin, Some(d("-450.00")), "free goods carry their full cost");
    assert_eq!(free.margin_percent, None, "no ratio against a zero amount");
    // order rollup: −100 + −450 = −550 over a 200 costed amount sum.
    assert_eq!(view.order_margin, Some(d("-550.00")));
    assert_eq!(view.order_margin_percent, Some(d("-275.00")));
}

// An unknown order id is a clean refusal on the margin read (no leak, no panic).
#[tokio::test]
async fn margin_view_of_unknown_order_refuses() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    assert!(matches!(
        w.order_margin_view(Uuid::new_v4()).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
}

// ── route-level probes ───────────────────────────────────────────────────────

const SECRET: &[u8] = b"selling-margin-probe-secret";

#[derive(serde::Serialize)]
struct TestClaims {
    sub: String,
    exp: usize,
    company_id: Uuid,
}
fn token(company_id: Uuid) -> String {
    let claims = TestClaims { sub: "margin-probe".into(), exp: 9_999_999_999, company_id };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(SECRET),
    ).unwrap()
}

async fn probe_app(costs: ScriptedCosts) -> axum::Router {
    let pool = pool().await;
    let m = backbone_selling::SellingModule::builder().with_database(pool.clone()).build().unwrap();
    backbone_selling::presentation::http::create_guarded_selling_routes(
        &m,
        pool,
        backbone_auth::company::CompanyVerifier::hs256(SECRET),
        Arc::new(costs),
        Arc::new(NoStockFulfillmentPort),
        Arc::new(NoServiceCatalog),
        Arc::new(NoServiceDelivery),
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

// A client CANNOT author a cost or a margin: the create body accepts no such field (injected JSON
// fields are ignored, not stored — `unit_cost`'s only writer is the confirm stamp), the draft
// lines carry NULL until confirm, and the margin route serves the computed figures only. The
// margin read is token-guarded like every other guarded route.
#[tokio::test]
async fn client_authored_cost_and_margin_injection_is_ignored() {
    let pool = pool().await;
    let company = Uuid::new_v4();
    let item = Uuid::new_v4();
    let app = probe_app(ScriptedCosts::healthy([(item, Some("100"))])).await;

    // The injected `unitCost`/`margin` fields ride the body; the API takes no notice.
    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-08-25","taxRate":"0",
             "lines":[{{"itemId":"{item}","quantity":"2","unitPrice":"500","unitCost":"9","margin":"1"}}]}}"#,
        uq("SO"), Uuid::new_v4(),
    );
    let (status, created) = send(app.clone(), "POST", "/sales-orders", Some(body), Some(company)).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let order: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();

    // Nothing was stored: the draft's lines carry no cost — the only writer is confirm's stamp.
    let pre: Vec<Option<Decimal>> =
        sqlx::query_scalar("SELECT unit_cost FROM selling.sales_order_items WHERE order_id=$1")
            .bind(order).fetch_all(&pool).await.unwrap();
    assert_eq!(pre, vec![None], "a client-authored cost must not persist on a draft");

    // The margin read is guarded: no token ⇒ 401.
    let (status, _) = send(app.clone(), "GET", &format!("/sales-orders/{order}/margin"), None, None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);

    // Confirm through the route (scripted port: cost 100), then read the computed margin.
    let (status, body) = send(
        app.clone(), "POST", "/sales-orders/confirm",
        Some(format!(r#"{{"orderId":"{order}"}}"#)), Some(company),
    ).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let (status, body) = send(app.clone(), "GET", &format!("/sales-orders/{order}/margin"), None, Some(company)).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");

    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    // Decimals arrive as JSON strings with the stored scale — compare numerically, not lexically.
    let dec = |v: &serde_json::Value| serde_json::from_value::<Decimal>(v.clone()).unwrap();
    assert_eq!(dec(&view["lines"][0]["unitCost"]), d("100"), "the stamp — not the injected 9");
    assert_eq!(dec(&view["lines"][0]["margin"]), d("800.00"), "1000.00 − 100·2");
    assert_eq!(dec(&view["lines"][0]["marginPercent"]), d("80.00"));
    assert_eq!(dec(&view["orderMargin"]), d("800.00"));
    assert_eq!(dec(&view["orderMarginPercent"]), d("80.00"));
    assert_eq!(view["marginLinesCosted"], 1);
    assert_eq!(view["marginLinesTotal"], 1);
}
