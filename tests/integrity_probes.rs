//! Route-level probes: the guarded surface validates creates and does NOT expose generic mutation
//! (create/update/delete/bulk) on selling documents — closing the CRUD-bypass. The module applies
//! no authn of its own (ADR-0029 posture): the composing service's org-session guard both
//! verifies the token and inserts the OrgContext the write handlers extract — an internal guard
//! would nest a second request-dedicated connection inside the composer's and shadow its fence
//! variables. The probe app stands the context in directly. Requires DATABASE_URL
//! (:5433/backbone_selling).
//!
//! IGC-*  the CRUD-bypass and validated-write invariants.
//! IGT-*  the token-tolerance invariants that remain provable at module level. The token-demand
//!        and cross-tenant data-isolation legs this suite once carried (unauthenticated-write
//!        refusal, claim-less-token refusal, foreign-id refusals on every verb, the
//!        restricted-role RLS probe) retired with the company strip (ADR-0029): the module ships
//!        no guard to demand anything, and an undecorated deployment is unfenced by design, so
//!        both authn and row isolation are proven by the composing service's probes plus the
//!        undecorated half-fence pin in tests/tenancy_posture_probe.rs.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::Serialize;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use backbone_selling::presentation::http::create_guarded_selling_routes;
use backbone_selling::SellingModule;

const SECRET: &[u8] = b"selling-integrity-probe-secret";

#[derive(Serialize)]
struct TestClaims {
    sub: String,
    exp: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    company_id: Option<Uuid>,
}

/// Mint an HS256 access token. `company_id = None` models a token that authenticates a user but
/// carries no tenant — it must not be allowed to write.
fn token(company_id: Option<Uuid>) -> String {
    let claims = TestClaims { sub: "probe-user".into(), exp: 9_999_999_999, company_id };
    encode(&Header::new(Algorithm::HS256), &claims, &EncodingKey::from_secret(SECRET)).unwrap()
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.unwrap()
}
async fn module(pool: &PgPool) -> SellingModule {
    SellingModule::builder().with_database(pool.clone()).build().unwrap()
}
fn app(pool: &PgPool, m: &SellingModule) -> axum::Router {
    create_guarded_selling_routes(
        m,
        pool.clone(),
        // No cost source in the probe app: resolves every cost to NULL, which confirm treats as
        // honest absence — good enough for the route-level probes here (the margin snapshot's own
        // port behaviors are proven in tests/margin_compute.rs with a scripted port).
        std::sync::Arc::new(backbone_selling::application::service::selling_unit_cost::NoUnitCostPort),
        // No stock engine in the probe app either: the opt-out adapter launches nothing and
        // reports no move figures — the stock port's own behaviors are proven in
        // tests/sale_stock_confirm.rs with a scripted port.
        std::sync::Arc::new(
            backbone_selling::application::service::selling_stock_fulfillment::NoStockFulfillmentPort,
        ),
        // No product-surface tracking and no project engine in the probe app: every line
        // reads as manually tracked and mints nothing — the service-delivery ports' own
        // behaviors are proven in tests/sale_service_confirm.rs with scripted ports.
        std::sync::Arc::new(
            backbone_selling::application::service::selling_service_catalog::NoServiceCatalog,
        ),
        std::sync::Arc::new(
            backbone_selling::application::service::selling_service_delivery::NoServiceDelivery,
        ),
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

/// Send a request with an optional bearer token.
async fn req_with(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
    bearer: Option<String>,
) -> (StatusCode, String) {
    let b = body.map(Body::from).unwrap_or(Body::empty());
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = app.oneshot(builder.body(b).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// Unauthenticated request.
async fn req(app: axum::Router, method: &str, uri: &str, body: Option<String>) -> (StatusCode, String) {
    req_with(app, method, uri, body, None).await
}

/// Request authenticated as some principal — the shape a composing service hands the module after
/// its org-session guard verifies the token and inserts OrgContext. The Bearer header models the
/// verified credential; nothing server-side reads it (the module is tenant-agnostic, ADR-0029),
/// and the composing service's tenancy decorator scopes whatever a request touches.
async fn req_as(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    req_with(app, method, uri, body, Some(token(Some(Uuid::new_v4())))).await
}

fn uq(p: &str) -> String { format!("{p}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]) }

// IGC-1: generic bulk create on invoices is NOT exposed on the guarded surface.
#[tokio::test]
async fn guarded_routes_lock_generic_invoice_bulk() {
    let pool = pool().await;
    let m = module(&pool).await;
    let (status, _) = req(app(&pool, &m), "POST", "/sales-invoices/bulk", Some("[]".into())).await;
    assert!(
        status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
        "generic bulk invoice create must not be exposed; got {status}"
    );
}

// IGC-2: generic soft-delete on an invoice is NOT exposed (no CRUD delete on the guarded surface).
#[tokio::test]
async fn guarded_routes_lock_generic_invoice_delete() {
    let pool = pool().await;
    let m = module(&pool).await;
    let id = uuid::Uuid::new_v4();
    let (status, _) = req(app(&pool, &m), "DELETE", &format!("/sales-invoices/{id}"), None).await;
    assert!(
        status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_FOUND,
        "generic invoice delete must not be exposed; got {status}"
    );
}

// (IGC-3/4/5 — the invoice-create validation probes — removed: the `/sales-invoices` validated
// create route is gone now that selling exited the invoice business; ADR-006. The AR invoice create
// + its validation live in backbone-billing.)

// (IGT-1/IGT-2 — the token-demand legs, "an unauthenticated write is rejected" and "a token
// without a company claim is rejected" — retired when the module dropped its internal auth guard:
// a module-level guard nests a second request-dedicated connection inside the composing service's
// and shadows its fence variables, blinding every org-scoped read on the write path. Authn —
// token verification and the session's org context — is the composing service's duty and is
// proven by its probes.)

// (IGT-4 — "a principal cannot confirm another tenant's order" — retired with the company strip
// (ADR-0029): the module no longer carries the tenant key the fence was cut on, and an undecorated
// deployment is unfenced by design. Row isolation is proven by the composing service's decorator
// probes; the module-side half-fence is pinned in tests/tenancy_posture_probe.rs.)

// IGT-3: a `companyId` smuggled in the body cannot break the write. Post-strip (ADR-0029) the
// selling tables carry no tenant column at all — the module cannot name a tenant, so the
// persisted-tenant-is-the-token's proof lives in the composing service's decorator probes. What
// this leg still proves at module level: the smuggled body field is TOLERATED (the create
// succeeds) and cannot corrupt the document. (Re-pointed from invoices to sales-orders when the
// invoice route was removed.)
#[tokio::test]
async fn body_company_id_cannot_override_the_token_tenant() {
    let pool = pool().await;
    let m = module(&pool).await;
    let number = uq("SO");
    let body = format!(
        r#"{{"orderNumber":"{}","companyId":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
        number, uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req_as(app(&pool, &m), "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);

    sqlx::query_scalar::<_, Uuid>("SELECT id FROM selling.sales_orders WHERE order_number = $1")
        .bind(&number)
        .fetch_one(&pool)
        .await
        .expect("order row created despite the smuggled body field");
}

// IGC-6: the quotation-template master is exposed on the guarded surface — create, list, and the
// duplicate-name refusal (422 `duplicate_template_name`, never a silent merge).
#[tokio::test]
async fn template_routes_create_list_and_refuse_duplicates() {
    let pool = pool().await;
    let m = module(&pool).await;
    let name = uq("Standard offer");

    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotation-templates",
        Some(format!(r#"{{"name":"{name}","validityDays":21,"defaultNotes":"Excludes VAT."}}"#)),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "template create: {body}");
    let id: Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = req_as(app(&pool, &m), "GET", "/quotation-templates", None).await;
    assert_eq!(status, StatusCode::OK);
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let found = list.as_array().unwrap().iter().find(|t| t["id"] == id.to_string()).expect("listed");
    assert_eq!(found["validityDays"], 21);
    assert_eq!(found["name"], name);

    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotation-templates",
        Some(format!(r#"{{"name":"{name}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "duplicate name must refuse 422: {body}");
    assert!(body.contains("duplicate_template_name"));

    // The duplicate refusal is module-global until a decorator composes: the module ships no
    // per-unit unique of its own (ADR-0029) — the service-side name pre-read is the only guard,
    // and a different token's create with the same live name refuses exactly the same way.
    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotation-templates",
        Some(format!(r#"{{"name":"{name}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "the live-name guard is module-global: {body}");
    assert!(body.contains("duplicate_template_name"));
}

// IGC-7: the invoicing-policy read model is served by its guarded route with the computed fields in
// camelCase — `qtyToInvoice`/`invoiceStatus` are read-time computes; no write route accepts them.
#[tokio::test]
async fn invoice_status_route_serves_the_policy_compute() {
    let pool = pool().await;
    let m = module(&pool).await;
    let item = uuid::Uuid::new_v4();

    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{item}","quantity":"10","unitPrice":"1000","invoicePolicy":"delivery"}}]}}"#,
        uq("SO"), uuid::Uuid::new_v4(),
    );
    let (status, created) = req_as(app(&pool, &m), "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let order_id: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, _) = req_as(
        app(&pool, &m), "POST", "/sales-orders/confirm",
        Some(format!(r#"{{"orderId":"{order_id}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = req_as(app(&pool, &m), "GET", &format!("/sales-orders/{order_id}/invoice-status"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let view: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(view["invoiceStatus"], "no", "delivery policy with zero delivery: nothing billable");
    assert_eq!(view["lines"][0]["invoicePolicy"], "delivery");
    assert_eq!(view["lines"][0]["qtyToInvoice"], "0");
    assert_eq!(view["lines"][0]["invoiceStatus"], "no");
}

// IGC-8: the order-line freeze holds through the route — on a confirmed order a priced-field PATCH
// refuses 422 `order_line_frozen` while a description-only PATCH succeeds.
#[tokio::test]
async fn line_freeze_holds_through_the_route() {
    let pool = pool().await;
    let m = module(&pool).await;
    let item = uuid::Uuid::new_v4();

    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{item}","quantity":"10","unitPrice":"1000"}}]}}"#,
        uq("SO"), uuid::Uuid::new_v4(),
    );
    let (status, created) = req_as(app(&pool, &m), "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let order_id: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    req_as(app(&pool, &m), "POST", "/sales-orders/confirm",
        Some(format!(r#"{{"orderId":"{order_id}"}}"#))).await;
    let line_id: Uuid = sqlx::query_scalar("SELECT id FROM selling.sales_order_items WHERE order_id=$1")
        .bind(order_id).fetch_one(&pool).await.unwrap();

    let (status, body) = req_as(
        app(&pool, &m), "PATCH", &format!("/sales-orders/lines/{line_id}"),
        Some(r#"{"quantity":"5"}"#.into()),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "frozen field must refuse: {body}");
    assert!(body.contains("order_line_frozen"));

    let (status, _) = req_as(
        app(&pool, &m), "PATCH", &format!("/sales-orders/lines/{line_id}"),
        Some(r#"{"description":"relabeled"}"#.into()),
    ).await;
    assert_eq!(status, StatusCode::OK, "description stays editable after confirmation");
}

// (IGT-5 — "a principal cannot move another tenant's quotation through its lifecycle" — retired
// with the company strip (ADR-0029): an undecorated deployment is unfenced by design. The
// machine-verb route coverage itself lives in IGT-6/IGT-7 and tests/quotation_machine.rs; row
// isolation is proven by the composing service's decorator probes and the half-fence pin in
// tests/tenancy_posture_probe.rs.)

// IGT-6: accept_quotation route transitions draft/sent → accepted and refuses a double accept.
#[tokio::test]
async fn accept_route_moves_draft_or_sent_to_accepted() {
    let pool = pool().await;
    let m = module(&pool).await;

    // Create a draft quotation.
    let (status, created) = req_as(
        app(&pool, &m), "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
            uq("QUO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let qid: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();

    // Happy path: accept from draft → accepted.
    let (status, _) = req_as(
        app(&pool, &m), "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "accepted");

    // Reset and test accept from sent → accepted.
    sqlx::query("UPDATE selling.quotations SET status='draft' WHERE id=$1").bind(qid).execute(&pool).await.unwrap();
    let (status, _) = req_as(
        app(&pool, &m), "POST", "/quotations/send",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = req_as(
        app(&pool, &m), "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "accepted");

    // Refusal: accept on already-accepted → 422 invalid_transition.
    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "already accepted must refuse: {body}");
    assert!(body.contains("invalid_transition") || body.contains("not_draft"));
}

// IGT-7: convert_quotation_to_order route transitions accepted → ordered and creates the order.
#[tokio::test]
async fn convert_route_transitions_accepted_to_ordered_and_creates_order() {
    let pool = pool().await;
    let m = module(&pool).await;

    // Create and accept a quotation.
    let (status, created) = req_as(
        app(&pool, &m), "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"2","unitPrice":"1500"}}]}}"#,
            uq("QUO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let qid: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, _) = req_as(
        app(&pool, &m), "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);

    // Happy path: convert accepted → creates order and marks quotation ordered.
    let order_number = uq("SO");
    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid}","orderNumber":"{order_number}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "convert must create order: {body}");
    let response: serde_json::Value = serde_json::from_str(&body).unwrap();
    let order_id: Uuid = response["orderId"].as_str().unwrap().parse().unwrap();
    assert_eq!(response["quotationId"].as_str().unwrap().parse::<Uuid>().unwrap(), qid);

    // Verify quotation is now ordered.
    let qst: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(qst, "ordered");

    // Verify order was created with the correct data.
    let ost: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order_id).fetch_one(&pool).await.unwrap();
    assert_eq!(ost, "draft");
    let qid_ref: Option<Uuid> = sqlx::query_scalar("SELECT quotation_id FROM selling.sales_orders WHERE id=$1")
        .bind(order_id).fetch_one(&pool).await.unwrap();
    assert_eq!(qid_ref, Some(qid));
    let line_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM selling.sales_order_items WHERE order_id=$1")
        .bind(order_id).fetch_one(&pool).await.unwrap();
    assert_eq!(line_count, 1, "lines must be copied from quotation");

    // Refusal: convert a non-accepted quotation → 422.
    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid}","orderNumber":"{}"}}"#, uq("SO"))),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "ordered quotation must refuse: {body}");
    assert!(body.contains("quotation_not_accepted") || body.contains("invalid_transition"));

    // Refusal: convert a draft quotation → 422.
    let (status, created2) = req_as(
        app(&pool, &m), "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
            uq("QUO2"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED);
    let qid2: Uuid = serde_json::from_str::<serde_json::Value>(&created2).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, body) = req_as(
        app(&pool, &m), "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid2}","orderNumber":"{}"}}"#, uq("SO"))),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "draft quotation must refuse: {body}");
    assert!(body.contains("quotation_not_accepted"));
}

// (IGT-8 — the restricted-role convert fence probe — retired with the company strip (ADR-0029):
// the module no longer declares the fence that probe pinned, and an undecorated deployment is
// unfenced by design — even the owner's own writes default-deny on a restricted role now. The
// half-fence posture itself (armed flags, zero module policies) is pinned in
// tests/tenancy_posture_probe.rs; row isolation belongs to the composing service's decorator
// probes.)
