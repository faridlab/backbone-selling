//! Route-level probes: the guarded surface validates creates and does NOT expose generic mutation
//! (create/update/delete/bulk) on selling documents — closing the CRUD-bypass — and every validated
//! write derives its tenant from a signed token rather than the request body. Requires
//! DATABASE_URL (:5433/backbone_selling).
//!
//! IGC-1..IGC-5  the CRUD-bypass and validated-write invariants.
//! IGT-1..IGT-3  the tenancy invariants (mirrors the TG-* cases backbone-pos proved).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use backbone_auth::tenant::TenantVerifier;
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
    create_guarded_selling_routes(m, pool.clone(), TenantVerifier::hs256(SECRET))
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

// IGC-3: the validated create endpoint accepts a well-formed invoice (201). No `companyId` in the
// body — the tenant rides on the token.
#[tokio::test]
async fn guarded_create_invoice_ok() {
    let pool = pool().await;
    let m = module(&pool).await;
    let (company, customer, ar, ppn, rev) = (
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let body = format!(
        r#"{{"invoiceNumber":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "taxRate":"11","receivableAccountId":"{}","taxOutputAccountId":"{}",
             "lines":[{{"itemId":"{}","revenueAccountId":"{}","quantity":"1","unitPrice":"1000000"}}]}}"#,
        uq("INV"), customer, ar, ppn, uuid::Uuid::new_v4(), rev,
    );
    let (status, _) = req_as(app(&pool, &m), company, "POST", "/sales-invoices", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
}

// IGC-4: the validated create endpoint rejects an empty invoice (422 empty_document).
#[tokio::test]
async fn guarded_create_invoice_rejects_empty() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"invoiceNumber":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "receivableAccountId":"{}","lines":[]}}"#,
        uq("INV"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, body) = req_as(
        app(&pool, &m), uuid::Uuid::new_v4(), "POST", "/sales-invoices", Some(body),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("empty_document"), "got: {body}");
}

// IGC-5: tax charged with no PPN output account is rejected (422 tax_account_missing).
#[tokio::test]
async fn guarded_create_invoice_rejects_missing_tax_account() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"invoiceNumber":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "taxRate":"11","receivableAccountId":"{}",
             "lines":[{{"itemId":"{}","revenueAccountId":"{}","quantity":"1","unitPrice":"1000000"}}]}}"#,
        uq("INV"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, body) = req_as(
        app(&pool, &m), uuid::Uuid::new_v4(), "POST", "/sales-invoices", Some(body),
    ).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("tax_account_missing"), "got: {body}");
}

// IGT-1: an unauthenticated write is rejected. Before the tenant guard this create succeeded and
// stamped whatever `companyId` the caller put in the body.
#[tokio::test]
async fn guarded_write_rejects_unauthenticated() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"invoiceNumber":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "receivableAccountId":"{}","lines":[]}}"#,
        uq("INV"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req(app(&pool, &m), "POST", "/sales-invoices", Some(body)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "an unauthenticated write must not reach the service");
}

// IGT-2: a token that authenticates a user but carries no `company_id` claim is rejected — a writer
// that cannot name its tenant must never run.
#[tokio::test]
async fn guarded_write_rejects_token_without_company_id() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"invoiceNumber":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "receivableAccountId":"{}","lines":[]}}"#,
        uq("INV"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req_with(
        app(&pool, &m), "POST", "/sales-invoices", Some(body), Some(token(None)),
    ).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a token with no tenant must not write");
}

// IGT-3: a `companyId` smuggled in the body is ignored — the persisted tenant is the token's. This is
// the regression that motivated the change: the body must not be able to name the tenant.
#[tokio::test]
async fn body_company_id_cannot_override_the_token_tenant() {
    let pool = pool().await;
    let m = module(&pool).await;
    let token_company = uuid::Uuid::new_v4();
    let attacker_company = uuid::Uuid::new_v4();
    let number = uq("INV");
    let body = format!(
        r#"{{"invoiceNumber":"{}","companyId":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "taxRate":"11","receivableAccountId":"{}","taxOutputAccountId":"{}",
             "lines":[{{"itemId":"{}","revenueAccountId":"{}","quantity":"1","unitPrice":"1000000"}}]}}"#,
        number, attacker_company, uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, _) = req_as(app(&pool, &m), token_company, "POST", "/sales-invoices", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);

    let persisted: Uuid =
        sqlx::query_scalar("SELECT company_id FROM selling.sales_invoices WHERE invoice_number = $1")
            .bind(&number)
            .fetch_one(&pool)
            .await
            .expect("invoice row");
    assert_eq!(persisted, token_company, "tenant must come from the token, not the body");
    assert_ne!(persisted, attacker_company, "the body's companyId must be ignored");
}
