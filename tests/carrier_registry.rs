//! The delivery-carrier registry — master-data verbs (create/update/list, deactivate-don't-delete)
//! and the order's carrier/tracking verb, at the service level and through the guarded routes.
//!
//! Requires DATABASE_URL (:5433/backbone_selling). REGISTRY ONLY is the fence: these tests pin the
//! master + the order link, which is also proof-by-absence that nothing here touches rates,
//! labels, or the DeliveryRequested envelope.
//!
//! Coverage map:
//!   create + module-global duplicate-live-name refusal (the per-unit unique is the composing
//!   decorator's — ADR-0029)
//!   update incl. clearing the tracking template; unknown id ⇒ carrier_not_found
//!   list active-only vs all (deactivated carriers stay readable)
//!   set-delivery: draft AND confirmed writable, cancelled refused, unknown carrier ⇒ clean 404,
//!     keep/clear/set patch semantics
//!   create-order with a carrier choice (validated pre-transaction)
//!   route probes: the registry verbs are served end-to-end (authn is the composing service's
//!     duty — the module applies no guard of its own)

use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use backbone_selling::application::service::selling_carrier::UpdateCarrierPatch;
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
fn line(item: Uuid) -> NewLine {
    NewLine { invoice_policy: None, is_downpayment: None,
        item_id: item, revenue_account_id: None, description: None,
        quantity: d("1"), unit_price: d("1000"), line_discount: d("0"),
    }
}
async fn draft_order(w: &SellingWriteService) -> Uuid {
    w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None, delivery_carrier_id: None, branch_id: None, customer_id: Uuid::new_v4(),
        order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 25).unwrap(),
        delivery_date: None, currency: None, tax_rate: d("0"), notes: None,
        lines: vec![line(Uuid::new_v4())],
    }).await.unwrap()
}

// C1: create a carrier; a duplicate live name refuses (module-global service-side pre-read —
// the module ships no unique of its own; under a composed tenancy decorator the decorator's
// per-unit unique is the hard backstop, ADR-0029). A soft-deleted name stays reusable.
#[tokio::test]
async fn create_refuses_live_duplicate_names() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let name = uq("SiCepat");

    let id = w.create_delivery_carrier(&name, Some("https://track/{ref}")).await.unwrap();
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT tracking_url_template FROM selling.delivery_carriers WHERE id=$1")
        .bind(id).fetch_one(&pool).await.unwrap();
    assert_eq!(stored.as_deref(), Some("https://track/{ref}"));

    match w.create_delivery_carrier(&name, None).await.unwrap_err() {
        SellingError::CarrierDuplicate(n) => assert_eq!(n, name),
        other => panic!("expected CarrierDuplicate, got {other:?}"),
    }
}

// C2: update the master fields — rename, flip active, set AND CLEAR the tracking template; an
// unknown id is a clean `carrier_not_found`, never an FK 500 or a silent no-op.
#[tokio::test]
async fn update_renames_deactivates_and_clears_the_template() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let id = w.create_delivery_carrier(&uq("JNE"), Some("https://jne/{ref}")).await.unwrap();

    w.update_delivery_carrier(id, UpdateCarrierPatch {
        name: Some(uq("JNE Express")),
        tracking_url_template: Some(Some("https://jne.co.id/{ref}".into())),
        ..Default::default()
    }).await.unwrap();
    let after = w.list_delivery_carriers(false).await.unwrap().into_iter().find(|c| c.id == id).unwrap();
    assert_eq!(after.tracking_url_template.as_deref(), Some("https://jne.co.id/{ref}"));

    // explicit CLEAR: Some(None) writes NULL; the field set is distinguished from "not asked".
    w.update_delivery_carrier(id, UpdateCarrierPatch {
        tracking_url_template: Some(None), ..Default::default()
    }).await.unwrap();
    let after = w.list_delivery_carriers(false).await.unwrap().into_iter().find(|c| c.id == id).unwrap();
    assert_eq!(after.tracking_url_template, None);

    // deactivate (the retirement path — no delete verb exists on the registry).
    w.update_delivery_carrier(id, UpdateCarrierPatch { active: Some(false), ..Default::default() })
        .await.unwrap();
    assert!(!w.list_delivery_carriers(false).await.unwrap().into_iter().find(|c| c.id == id).unwrap().active);

    // an unknown id refuses cleanly (under a composed decorator a foreign id is
    // indistinguishable — which is the point).
    assert!(matches!(
        w.update_delivery_carrier(Uuid::new_v4(), UpdateCarrierPatch { name: Some("x".into()), ..Default::default() }).await.unwrap_err(),
        SellingError::CarrierNotFound(_)
    ));
}

// C3: the list serves the active set by default; `active_only = false` keeps the retired carriers
// readable (history must not vanish when a carrier is deactivated).
#[tokio::test]
async fn list_defaults_to_active_and_can_include_retired() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let live = w.create_delivery_carrier(&uq("Live"), None).await.unwrap();
    let retired = w.create_delivery_carrier(&uq("Retired"), None).await.unwrap();
    w.update_delivery_carrier(retired, UpdateCarrierPatch { active: Some(false), ..Default::default() })
        .await.unwrap();

    let ids_of = |rows: Vec<backbone_selling::application::service::selling_carrier::CarrierDto>| {
        rows.into_iter().map(|c| c.id).collect::<Vec<_>>()
    };
    let active = ids_of(w.list_delivery_carriers(true).await.unwrap());
    assert!(active.contains(&live));
    assert!(!active.contains(&retired), "the retired carrier leaves the default pick-list");
    let all = ids_of(w.list_delivery_carriers(false).await.unwrap());
    assert!(all.contains(&live) && all.contains(&retired), "history stays readable");
}

// C4: set-delivery writes fulfillment metadata — writable on draft AND confirmed, refused only on
// cancelled; an unknown carrier refuses `carrier_not_found` BEFORE any order write; the patch
// semantics keep/clear/set both fields independently.
#[tokio::test]
async fn set_delivery_writes_draft_and_confirmed_and_refuses_cancelled() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let carrier = w.create_delivery_carrier(&uq("AnterAja"), None).await.unwrap();

    // draft: set both.
    let draft = draft_order(&w).await;
    w.set_order_delivery(draft, Some(Some(carrier)), Some(Some("JTR-001".into()))).await.unwrap();
    let (cid, tref): (Option<Uuid>, Option<String>) = sqlx::query(
        "SELECT delivery_carrier_id, tracking_ref FROM selling.sales_orders WHERE id=$1")
        .bind(draft).fetch_one(&pool).await.map(|r| (r.get("delivery_carrier_id"), r.get("tracking_ref"))).unwrap();
    assert_eq!(cid, Some(carrier));
    assert_eq!(tref.as_deref(), Some("JTR-001"));

    // keep/clear/set: a tracking-only update KEEPS the carrier; a null CLEARS it.
    w.set_order_delivery(draft, None, Some(Some("JTR-002".into()))).await.unwrap();
    let (cid, tref): (Option<Uuid>, Option<String>) = sqlx::query(
        "SELECT delivery_carrier_id, tracking_ref FROM selling.sales_orders WHERE id=$1")
        .bind(draft).fetch_one(&pool).await.map(|r| (r.get("delivery_carrier_id"), r.get("tracking_ref"))).unwrap();
    assert_eq!(cid, Some(carrier), "an unasked field keeps its stored value");
    assert_eq!(tref.as_deref(), Some("JTR-002"));
    w.set_order_delivery(draft, Some(None), None).await.unwrap();
    let (cid, tref): (Option<Uuid>, Option<String>) = sqlx::query(
        "SELECT delivery_carrier_id, tracking_ref FROM selling.sales_orders WHERE id=$1")
        .bind(draft).fetch_one(&pool).await.map(|r| (r.get("delivery_carrier_id"), r.get("tracking_ref"))).unwrap();
    assert_eq!(cid, None, "an explicit null clears the carrier");
    assert_eq!(tref.as_deref(), Some("JTR-002"));

    // confirmed: still writable — tracking typically arrives only after ship.
    let confirmed = draft_order(&w).await;
    w.confirm_sales_order(confirmed, &NoUnitCostPort, &NoStockFulfillmentPort, &NoServiceCatalog, &NoServiceDelivery).await.unwrap();
    w.set_order_delivery(confirmed, Some(Some(carrier)), Some(Some("SHIP-9".into()))).await.unwrap();

    // cancelled: refused.
    let cancelled = draft_order(&w).await;
    w.cancel_sales_order(cancelled, &NoStockFulfillmentPort).await.unwrap();
    match w.set_order_delivery(cancelled, Some(Some(carrier)), None).await.unwrap_err() {
        SellingError::InvalidTransition { verb, current } => {
            assert_eq!(verb, "set_delivery");
            assert_eq!(current, "cancelled");
        }
        other => panic!("expected InvalidTransition, got {other:?}"),
    }

    // unknown carrier ⇒ clean refusal (pre-read, never the FK violation's 500); unknown order too.
    assert!(matches!(
        w.set_order_delivery(draft, Some(Some(Uuid::new_v4())), None).await.unwrap_err(),
        SellingError::CarrierNotFound(_)
    ));
    assert!(matches!(
        w.set_order_delivery(Uuid::new_v4(), Some(Some(carrier)), None).await.unwrap_err(),
        SellingError::OrderNotFound(_)
    ));
}

// C5: a create-time carrier choice is validated BEFORE the order transaction — an unknown
// carrier id refuses `carrier_not_found` and leaves no order row behind.
#[tokio::test]
async fn create_order_validates_the_carrier_choice() {
    let pool = pool().await;
    let w = SellingWriteService::new(pool.clone());
    let carrier = w.create_delivery_carrier(&uq("Ninja"), None).await.unwrap();
    let number = uq("SO");

    let err = w.create_sales_order(NewSalesOrder {
        order_number: number.clone(), quotation_id: None,
        delivery_carrier_id: Some(Uuid::new_v4()), // unknown carrier
        branch_id: None, customer_id: Uuid::new_v4(),
        order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 25).unwrap(),
        delivery_date: None, currency: None, tax_rate: d("0"), notes: None,
        lines: vec![line(Uuid::new_v4())],
    }).await.unwrap_err();
    assert!(matches!(err, SellingError::CarrierNotFound(_)));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM selling.sales_orders WHERE order_number=$1")
        .bind(&number).fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 0, "a refused create must leave no order behind");

    // the happy path persists the choice.
    let oid = w.create_sales_order(NewSalesOrder {
        order_number: uq("SO"), quotation_id: None,
        delivery_carrier_id: Some(carrier), branch_id: None, customer_id: Uuid::new_v4(),
        order_date: chrono::NaiveDate::from_ymd_opt(2026, 8, 25).unwrap(),
        delivery_date: None, currency: None, tax_rate: d("0"), notes: None,
        lines: vec![line(Uuid::new_v4())],
    }).await.unwrap();
    let cid: Option<Uuid> = sqlx::query_scalar("SELECT delivery_carrier_id FROM selling.sales_orders WHERE id=$1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(cid, Some(carrier));
}

// ── route-level probes ───────────────────────────────────────────────────────

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

// C6: the registry's verbs ride the module surface end-to-end — create, list (active-only
// default), deactivate-by-patch, duplicate refusal, set-delivery. Token-demand is not probed
// here: the module applies no auth of its own (an internal guard would shadow the composer's
// fence variables), so the composing service's org-session guard owns that property.
#[tokio::test]
async fn carrier_routes_serve_the_registry() {
    let pool = pool().await;
    let app = probe_app().await;

    // create + duplicate refusal.
    let name = uq("ProbeCarrier");
    let (status, created) = send(
        app.clone(), "POST", "/delivery-carriers",
        Some(format!(r#"{{"name":"{name}","trackingUrlTemplate":"https://t/{{ref}}"}}"#)),
    ).await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");
    let id: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = send(
        app.clone(), "POST", "/delivery-carriers", Some(format!(r#"{{"name":"{name}"}}"#)),
    ).await;
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("duplicate_carrier_name"));

    // the list serves the registry's carriers with the template in camelCase.
    let (status, body) = send(app.clone(), "GET", "/delivery-carriers", None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mine = list.as_array().unwrap().iter().find(|c| c["id"] == id.to_string()).expect("listed");
    assert_eq!(mine["name"], name);
    assert_eq!(mine["trackingUrlTemplate"], "https://t/{ref}");
    assert_eq!(mine["active"], true);

    // deactivate through the route; the default list drops it, activeOnly=false keeps it.
    let (status, body) = send(
        app.clone(), "PATCH", &format!("/delivery-carriers/{id}"), Some(r#"{"active":false}"#.into()),
    ).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let (status, body) = send(app.clone(), "GET", "/delivery-carriers", None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(list.as_array().unwrap().iter().all(|c| c["id"] != id.to_string()), "retired leaves the default list");
    let (status, body) = send(app.clone(), "GET", "/delivery-carriers?activeOnly=false", None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(list.as_array().unwrap().iter().any(|c| c["id"] == id.to_string()), "history stays readable");

    // the generic CRUD read router for the registry is mounted at its framework path.
    let (status, _) = send(app.clone(), "GET", "/delivery_carriers", None).await;
    assert_eq!(status, axum::http::StatusCode::OK, "the generic read router mounts at /delivery_carriers");

    // set-delivery through the route: happy path on a draft order.
    let w = SellingWriteService::new(pool.clone());
    let order = draft_order(&w).await;
    let (status, body) = send(
        app.clone(), "POST", "/sales-orders/set-delivery",
        Some(format!(r#"{{"orderId":"{order}","deliveryCarrierId":"{id}","trackingRef":"RT-1"}}"#)),
    ).await;
    // NOTE: the carrier was deactivated above — set-delivery only needs the row to EXIST (the
    // registry keeps retired carriers usable for history), so this is 200 by design.
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    let tref: Option<String> = sqlx::query_scalar("SELECT tracking_ref FROM selling.sales_orders WHERE id=$1")
        .bind(order).fetch_one(&pool).await.unwrap();
    assert_eq!(tref.as_deref(), Some("RT-1"));
}
