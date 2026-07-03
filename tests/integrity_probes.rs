//! Route-level probes: the guarded surface validates creates and does NOT expose generic mutation
//! (create/update/delete/bulk) on selling documents — closing the CRUD-bypass. Requires
//! DATABASE_URL (:5433/backbone_selling).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;

use backbone_selling::presentation::http::create_guarded_selling_routes;
use backbone_selling::SellingModule;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_selling".to_string());
    PgPool::connect(&url).await.unwrap()
}
async fn module(pool: &PgPool) -> SellingModule {
    SellingModule::builder().with_database(pool.clone()).build().unwrap()
}
fn app(pool: &PgPool, m: &SellingModule) -> axum::Router {
    create_guarded_selling_routes(m, pool.clone())
}
async fn req(app: axum::Router, method: &str, uri: &str, body: Option<String>) -> (StatusCode, String) {
    let b = body.map(Body::from).unwrap_or(Body::empty());
    let resp = app
        .oneshot(Request::builder().method(method).uri(uri).header("content-type", "application/json").body(b).unwrap())
        .await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
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

// IGC-3: the validated create endpoint accepts a well-formed invoice (201).
#[tokio::test]
async fn guarded_create_invoice_ok() {
    let pool = pool().await;
    let m = module(&pool).await;
    let (company, customer, ar, ppn, rev) = (
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let body = format!(
        r#"{{"invoiceNumber":"{}","companyId":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "taxRate":"11","receivableAccountId":"{}","taxOutputAccountId":"{}",
             "lines":[{{"itemId":"{}","revenueAccountId":"{}","quantity":"1","unitPrice":"1000000"}}]}}"#,
        uq("INV"), company, customer, ar, ppn, uuid::Uuid::new_v4(), rev,
    );
    let (status, _) = req(app(&pool, &m), "POST", "/sales-invoices", Some(body)).await;
    assert_eq!(status, StatusCode::CREATED);
}

// IGC-4: the validated create endpoint rejects an empty invoice (422 empty_document).
#[tokio::test]
async fn guarded_create_invoice_rejects_empty() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"invoiceNumber":"{}","companyId":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "receivableAccountId":"{}","lines":[]}}"#,
        uq("INV"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, body) = req(app(&pool, &m), "POST", "/sales-invoices", Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("empty_document"), "got: {body}");
}

// IGC-5: tax charged with no PPN output account is rejected (422 tax_account_missing).
#[tokio::test]
async fn guarded_create_invoice_rejects_missing_tax_account() {
    let pool = pool().await;
    let m = module(&pool).await;
    let body = format!(
        r#"{{"invoiceNumber":"{}","companyId":"{}","customerId":"{}","invoiceDate":"2026-07-03",
             "taxRate":"11","receivableAccountId":"{}",
             "lines":[{{"itemId":"{}","revenueAccountId":"{}","quantity":"1","unitPrice":"1000000"}}]}}"#,
        uq("INV"), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
        uuid::Uuid::new_v4(), uuid::Uuid::new_v4(),
    );
    let (status, body) = req(app(&pool, &m), "POST", "/sales-invoices", Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.contains("tax_account_missing"), "got: {body}");
}
