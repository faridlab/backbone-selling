//! Route-level probes: the guarded surface validates creates and does NOT expose generic mutation
//! (create/update/delete/bulk) on selling documents — closing the CRUD-bypass — and every validated
//! write derives its tenant from a signed token rather than the request body. Requires
//! DATABASE_URL (:5433/backbone_selling).
//!
//! IGC-1..IGC-8  the CRUD-bypass and validated-write invariants.
//! IGT-1..IGT-8  the tenancy invariants (mirrors the TG-* cases backbone-pos proved). IGT-8 runs
//!               the app on a restricted (non-BYPASSRLS) probe role — see its comment for why.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use backbone_auth::company::CompanyVerifier;
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
        CompanyVerifier::hs256(SECRET),
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
    )
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

/// Request authenticated as a principal of `company`.
async fn req_as(
    app: axum::Router,
    company: Uuid,
    method: &str,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, String) {
    req_with(app, method, uri, body, Some(token(Some(company)))).await
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

// IGT-1: an unauthenticated write is rejected. Before the tenant guard this create succeeded and
// stamped whatever `companyId` the caller put in the body. (Re-pointed from invoices to sales-orders
// when the invoice route was removed.)
#[tokio::test]
async fn guarded_write_rejects_unauthenticated() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
        uq("SO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req(app(&pool, &m), "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "an unauthenticated write must not reach the service");
}

// IGT-2: a token that authenticates a user but carries no `company_id` claim is rejected — a writer
// that cannot name its tenant must never run.
#[tokio::test]
async fn guarded_write_rejects_token_without_company_id() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
        uq("SO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req_with(
        app(&pool, &m), "POST", "/sales-orders", Some(body), Some(token(None)),
    ).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a token with no tenant must not write");
}

// IGT-4: authentication is not ownership. A principal of company A must not be able to confirm
// company B's order by knowing its id — that would fire B's downstream billing and GL posting from
// A's token. The tenant scopes the row lookup, so a foreign order is indistinguishable from a
// missing one (404-shaped `not_draft`), which also avoids leaking whether the id exists.
#[tokio::test]
async fn a_principal_cannot_confirm_another_tenants_order() {
    let pool = pool().await;
    let m = module(&pool).await;
    let victim_company = uuid::Uuid::new_v4();
    let attacker_company = uuid::Uuid::new_v4();

    // The victim's order, created legitimately under the victim's own token.
    let number = uq("SO");
    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{}","revenueAccountId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
        number, uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, created) = req_as(app(&pool, &m), victim_company, "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "victim order should be created: {created}");
    let order_id: Uuid = serde_json::from_str::<serde_json::Value>(&created)
        .unwrap()["id"].as_str().unwrap().parse().unwrap();

    // The attacker authenticates as their own tenant and aims at the victim's order id.
    let (status, _) = req_as(
        app(&pool, &m), attacker_company, "POST", "/sales-orders/confirm",
        Some(format!(r#"{{"orderId":"{order_id}"}}"#)),
    ).await;
    assert_ne!(status, StatusCode::OK, "a foreign tenant must not confirm this order");

    // And the order is untouched — still draft, not advanced into the billing flow.
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.sales_orders WHERE id=$1")
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .expect("order row");
    assert_eq!(st, "draft", "the victim's order must remain draft after a foreign confirm attempt");
}

// IGT-3: a `companyId` smuggled in the body is ignored — the persisted tenant is the token's. This is
// the regression that motivated the change: the body must not be able to name the tenant. (Re-pointed
// from invoices to sales-orders when the invoice route was removed.)
#[tokio::test]
async fn body_company_id_cannot_override_the_token_tenant() {
    let pool = pool().await;
    let m = module(&pool).await;
    let token_company = uuid::Uuid::new_v4();
    let attacker_company = uuid::Uuid::new_v4();
    let number = uq("SO");
    let body = format!(
        r#"{{"orderNumber":"{}","companyId":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
        number, attacker_company, uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req_as(app(&pool, &m), token_company, "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);

    let persisted: Uuid =
        sqlx::query_scalar("SELECT company_id FROM selling.sales_orders WHERE order_number = $1")
            .bind(&number)
            .fetch_one(&pool)
            .await
            .expect("order row");
    assert_eq!(persisted, token_company, "tenant must come from the token, not the body");
    assert_ne!(persisted, attacker_company, "the body's companyId must be ignored");
}

// IGC-6: the quotation-template master is exposed on the guarded surface — create, list, and the
// per-tenant duplicate-name refusal (422 `duplicate_template_name`, never a silent merge).
#[tokio::test]
async fn template_routes_create_list_and_refuse_duplicates() {
    let pool = pool().await;
    let m = module(&pool).await;
    let company = uuid::Uuid::new_v4();
    let name = uq("Standard offer");

    let (status, body) = req_as(
        app(&pool, &m), company, "POST", "/quotation-templates",
        Some(format!(r#"{{"name":"{name}","validityDays":21,"defaultNotes":"Excludes VAT."}}"#)),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "template create: {body}");
    let id: Uuid = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"].as_str().unwrap().parse().unwrap();

    let (status, body) = req_as(app(&pool, &m), company, "GET", "/quotation-templates", None).await;
    assert_eq!(status, StatusCode::OK);
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    let found = list.as_array().unwrap().iter().find(|t| t["id"] == id.to_string()).expect("listed");
    assert_eq!(found["validityDays"], 21);
    assert_eq!(found["name"], name);

    let (status, body) = req_as(
        app(&pool, &m), company, "POST", "/quotation-templates",
        Some(format!(r#"{{"name":"{name}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "duplicate name must refuse 422: {body}");
    assert!(body.contains("duplicate_template_name"));

    // A template never crosses tenants on the wire.
    let (status, _) = req_as(
        app(&pool, &m), uuid::Uuid::new_v4(), "POST", "/quotation-templates",
        Some(format!(r#"{{"name":"{name}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "the unique index is per company");
}

// IGC-7: the invoicing-policy read model is served by its guarded route with the computed fields in
// camelCase — `qtyToInvoice`/`invoiceStatus` are read-time computes; no write route accepts them.
#[tokio::test]
async fn invoice_status_route_serves_the_policy_compute() {
    let pool = pool().await;
    let m = module(&pool).await;
    let company = uuid::Uuid::new_v4();
    let item = uuid::Uuid::new_v4();

    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{item}","quantity":"10","unitPrice":"1000","invoicePolicy":"delivery"}}]}}"#,
        uq("SO"), uuid::Uuid::new_v4(),
    );
    let (status, created) = req_as(app(&pool, &m), company, "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let order_id: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, _) = req_as(
        app(&pool, &m), company, "POST", "/sales-orders/confirm",
        Some(format!(r#"{{"orderId":"{order_id}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = req_as(app(&pool, &m), company, "GET", &format!("/sales-orders/{order_id}/invoice-status"), None).await;
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
    let company = uuid::Uuid::new_v4();
    let item = uuid::Uuid::new_v4();

    let body = format!(
        r#"{{"orderNumber":"{}","customerId":"{}","orderDate":"2026-07-03","taxRate":"0",
             "lines":[{{"itemId":"{item}","quantity":"10","unitPrice":"1000"}}]}}"#,
        uq("SO"), uuid::Uuid::new_v4(),
    );
    let (status, created) = req_as(app(&pool, &m), company, "POST", "/sales-orders", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let order_id: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    req_as(app(&pool, &m), company, "POST", "/sales-orders/confirm",
        Some(format!(r#"{{"orderId":"{order_id}"}}"#))).await;
    let line_id: Uuid = sqlx::query_scalar("SELECT id FROM selling.sales_order_items WHERE order_id=$1")
        .bind(order_id).fetch_one(&pool).await.unwrap();

    let (status, body) = req_as(
        app(&pool, &m), company, "PATCH", &format!("/sales-orders/lines/{line_id}"),
        Some(r#"{"quantity":"5"}"#.into()),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "frozen field must refuse: {body}");
    assert!(body.contains("order_line_frozen"));

    let (status, _) = req_as(
        app(&pool, &m), company, "PATCH", &format!("/sales-orders/lines/{line_id}"),
        Some(r#"{"description":"relabeled"}"#.into()),
    ).await;
    assert_eq!(status, StatusCode::OK, "description stays editable after confirmation");
}

// IGT-5: the machine verbs are tenant-scoped — a principal of company A cannot move company B's
// quotation through its lifecycle by knowing the id.
#[tokio::test]
async fn a_principal_cannot_send_another_tenants_quotation() {
    let pool = pool().await;
    let m = module(&pool).await;
    let victim = uuid::Uuid::new_v4();
    let attacker = uuid::Uuid::new_v4();

    let (status, created) = req_as(
        app(&pool, &m), victim, "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
            uq("QUO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let qid: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();

    for (uri, body) in [
        ("/quotations/send", format!(r#"{{"quotationId":"{qid}"}}"#)),
        ("/quotations/cancel", format!(r#"{{"quotationId":"{qid}"}}"#)),
        ("/quotations/re-draft", format!(r#"{{"quotationId":"{qid}"}}"#)),
    ] {
        let (status, _) = req_as(app(&pool, &m), attacker, "POST", uri, Some(body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "a foreign machine verb must not find the quotation");
    }
    // the victim's quotation is untouched.
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "draft");

    // and the owner's own verb works through the route.
    let (status, _) = req_as(
        app(&pool, &m), victim, "POST", "/quotations/send",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
}

// IGT-6: accept_quotation route transitions draft/sent → accepted and is tenant-scoped.
#[tokio::test]
async fn accept_route_moves_draft_or_sent_to_accepted_and_is_tenant_scoped() {
    let pool = pool().await;
    let m = module(&pool).await;
    let victim = uuid::Uuid::new_v4();
    let attacker = uuid::Uuid::new_v4();

    // Create a draft quotation as the victim.
    let (status, created) = req_as(
        app(&pool, &m), victim, "POST", "/quotations",
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
        app(&pool, &m), victim, "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "accepted");

    // Reset and test accept from sent → accepted.
    sqlx::query("UPDATE selling.quotations SET status='draft' WHERE id=$1").bind(qid).execute(&pool).await.unwrap();
    let (status, _) = req_as(
        app(&pool, &m), victim, "POST", "/quotations/send",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = req_as(
        app(&pool, &m), victim, "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&pool).await.unwrap();
    assert_eq!(st, "accepted");

    // Refusal: accept on already-accepted → 422 invalid_transition.
    let (status, body) = req_as(
        app(&pool, &m), victim, "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "already accepted must refuse: {body}");
    assert!(body.contains("invalid_transition") || body.contains("not_draft"));

    // Tenant scoping: attacker cannot accept victim's quotation.
    let (status, _) = req_as(
        app(&pool, &m), attacker, "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "foreign tenant must not find the quotation");
}

// IGT-7: convert_quotation_to_order route transitions accepted → ordered and creates the order.
#[tokio::test]
async fn convert_route_transitions_accepted_to_ordered_and_creates_order() {
    let pool = pool().await;
    let m = module(&pool).await;
    let company = uuid::Uuid::new_v4();

    // Create and accept a quotation.
    let (status, created) = req_as(
        app(&pool, &m), company, "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"2","unitPrice":"1500"}}]}}"#,
            uq("QUO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let qid: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, _) = req_as(
        app(&pool, &m), company, "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK);

    // Happy path: convert accepted → creates order and marks quotation ordered.
    let order_number = uq("SO");
    let (status, body) = req_as(
        app(&pool, &m), company, "POST", "/quotations/convert-to-order",
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
        app(&pool, &m), company, "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid}","orderNumber":"{}"}}"#, uq("SO"))),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "ordered quotation must refuse: {body}");
    assert!(body.contains("quotation_not_accepted") || body.contains("invalid_transition"));

    // Refusal: convert a draft quotation → 422.
    let (status, created2) = req_as(
        app(&pool, &m), company, "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
            uq("QUO2"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED);
    let qid2: Uuid = serde_json::from_str::<serde_json::Value>(&created2).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, body) = req_as(
        app(&pool, &m), company, "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid2}","orderNumber":"{}"}}"#, uq("SO"))),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "draft quotation must refuse: {body}");
    assert!(body.contains("quotation_not_accepted"));
}

// The restricted probe role for the RLS-dependent probe below: NOSUPERUSER NOBYPASSRLS — the only
// session posture under which Row-Level Security policies actually bind (superusers and BYPASSRLS
// roles always bypass them). Minted idempotently by the admin test pool, following the same pattern
// as backbone-billing's fence suite.
const PROBE_ROLE: &str = "selling_fence_probe";
const PROBE_PASSWORD: &str = "probe";

/// Rebuild DATABASE_URL aimed at the probe role, keeping its host/port/database.
fn restricted_url(admin_url: &str) -> String {
    let rest = admin_url
        .trim_start_matches("postgresql://")
        .trim_start_matches("postgres://");
    let (authority, path) = rest.split_once('/').expect("DATABASE_URL must name a database");
    // Drop any userinfo before the host (take the LAST '@' so IPv6 literals cannot confuse it).
    let hostport = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let db = path.split('?').next().unwrap_or("backbone_selling");
    format!("postgresql://{PROBE_ROLE}:{PROBE_PASSWORD}@{hostport}/{db}")
}

/// A pool connected as the restricted probe role, minted and granted by the admin pool.
async fn restricted_pool(admin: &PgPool) -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    let db = url
        .trim_start_matches("postgresql://")
        .trim_start_matches("postgres://")
        .split_once('/')
        .and_then(|(_, path)| path.split('?').next())
        .unwrap_or("backbone_selling")
        .to_string();

    // Serialize mint + grants across parallel tests (shared-catalog DDL does not tolerate
    // concurrent GRANTs), then tolerate losing the race — the winner made the same role.
    sqlx::query("SELECT pg_advisory_lock(hashtext('selling_fence_probe'))")
        .execute(admin)
        .await
        .expect("take probe mint lock");
    // Tolerate losing the race (or a prior run's role): a duplicate-role error here means the
    // role already exists with the same shape; a real failure surfaces at the GRANTs below.
    let _ = sqlx::query(&format!(
        "CREATE ROLE {PROBE_ROLE} LOGIN PASSWORD '{PROBE_PASSWORD}' \
           NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE"
    ))
    .execute(admin)
    .await;
    // One statement per execute (a multi-command string is not a legal prepared statement). The
    // grants cover exactly the document tables the guarded write path touches — no more.
    for grant in [
        format!(r#"GRANT CONNECT ON DATABASE "{db}" TO {PROBE_ROLE}"#),
        format!("GRANT USAGE ON SCHEMA selling TO {PROBE_ROLE}"),
        format!("GRANT SELECT, INSERT, UPDATE ON TABLE selling.quotations TO {PROBE_ROLE}"),
        format!("GRANT SELECT, INSERT, UPDATE ON TABLE selling.quotation_items TO {PROBE_ROLE}"),
        format!("GRANT SELECT, INSERT, UPDATE ON TABLE selling.sales_orders TO {PROBE_ROLE}"),
        format!("GRANT SELECT, INSERT, UPDATE ON TABLE selling.sales_order_items TO {PROBE_ROLE}"),
    ] {
        sqlx::query(&grant).execute(admin).await.expect("grant probe role");
    }
    sqlx::query("SELECT pg_advisory_unlock(hashtext('selling_fence_probe'))")
        .execute(admin)
        .await
        .expect("release probe mint lock");

    PgPool::connect(&restricted_url(&url)).await.expect("connect as restricted probe")
}

// IGT-8: convert-to-order is tenant-fenced. Unlike the machine verbs above — whose guarded
// statements carry an explicit `company_id` filter as defense-in-depth — the conversion source read
// is ID-only (no company in the query text), so its cross-tenant fence is Row-Level Security alone.
// RLS only binds for a non-BYPASSRLS session, so this probe runs the whole app on the restricted
// probe role: under it, a foreign tenant aiming at an accepted quotation must get a 404 and the
// victim's quotation must stay accepted with no order derived. (Run against a superuser pool the
// same request would SUCCEED — superusers bypass RLS — which is why the deployment contract
// requires the app to connect as a non-superuser role.)
#[tokio::test]
async fn a_principal_cannot_convert_another_tenants_quotation() {
    let admin = pool().await;
    let restricted = restricted_pool(&admin).await;
    let m = module(&restricted).await;
    let victim = uuid::Uuid::new_v4();
    let attacker = uuid::Uuid::new_v4();

    // The victim's quotation, created and accepted through the route on the restricted pool.
    let (status, created) = req_as(
        app(&restricted, &m), victim, "POST", "/quotations",
        Some(format!(
            r#"{{"quotationNumber":"{}","customerId":"{}","quotationDate":"2026-07-03","taxRate":"0",
                 "lines":[{{"itemId":"{}","quantity":"1","unitPrice":"1000"}}]}}"#,
            uq("QUO"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        )),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let qid: Uuid = serde_json::from_str::<serde_json::Value>(&created).unwrap()["id"].as_str().unwrap().parse().unwrap();
    let (status, body) = req_as(
        app(&restricted, &m), victim, "POST", "/quotations/accept",
        Some(format!(r#"{{"quotationId":"{qid}"}}"#)),
    ).await;
    assert_eq!(status, StatusCode::OK, "owner accepts on the restricted pool: {body}");

    // The attacker aims at the victim's accepted quotation.
    let (status, body) = req_as(
        app(&restricted, &m), attacker, "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid}","orderNumber":"{}"}}"#, uq("SO"))),
    ).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a foreign convert must not find the quotation: {body}");
    assert!(body.contains("quotation_not_found"), "{body}");

    // The victim's quotation is untouched — still accepted, no order derived from it.
    let st: String = sqlx::query_scalar("SELECT status::text FROM selling.quotations WHERE id=$1")
        .bind(qid).fetch_one(&admin).await.unwrap();
    assert_eq!(st, "accepted");
    let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM selling.sales_orders WHERE quotation_id=$1")
        .bind(qid).fetch_one(&admin).await.unwrap();
    assert_eq!(orders, 0, "a refused convert must not derive an order");

    // And the owner's own convert still works on the restricted pool.
    let (status, body) = req_as(
        app(&restricted, &m), victim, "POST", "/quotations/convert-to-order",
        Some(format!(r#"{{"quotationId":"{qid}","orderNumber":"{}"}}"#, uq("SO"))),
    ).await;
    assert_eq!(status, StatusCode::CREATED, "owner converts: {body}");
}
