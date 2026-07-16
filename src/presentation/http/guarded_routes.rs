//! Guarded route composition — the RECOMMENDED way to mount the selling module.
//!
//! Hand-authored (user-owned; see `metaphor.codegen.yaml`). Selling documents (quotation / sales
//! order / sales invoice) are read + **validated create**; the generic create/update/delete CRUD
//! is NOT mounted, so a caller cannot write an invoice with an inconsistent `total`, no lines, or a
//! server-computed field it shouldn't set. Line amounts + document totals are computed server-side.
//!
//! The GL-posting seam (`post_sales_invoice`) is intentionally **not** an HTTP route here: it needs
//! a `GlPostSink` supplied by the composing service (the accounting adapter). It is driven by the
//! service layer / a posting job and proven by the seam integration test.
//!
//! `SellingWriteService` is stateless over the pool, so it is constructed here rather than pulled
//! from the generated `SellingModule` struct — the guarded surface survives a regen of the module.

use std::sync::Arc;

use axum::{
    extract::State, http::StatusCode, middleware::from_fn_with_state, response::IntoResponse,
    routing::post, Json, Router,
};
use backbone_auth::company::{company_auth, CompanyContext, CompanyVerifier};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::application::service::selling_write_service::{
    NewLine, NewQuotation, NewSalesInvoice, NewSalesOrder, SellingError, SellingWriteService,
};
use crate::SellingModule;

use super::{
    create_quotation_read_routes, create_quotation_item_read_routes,
    create_sales_invoice_read_routes, create_sales_invoice_item_read_routes,
    create_sales_order_read_routes, create_sales_order_item_read_routes,
};

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    message: String,
}
#[derive(Debug, Serialize)]
struct IdResponse {
    id: Uuid,
}
fn err_response(e: SellingError) -> axum::response::Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(ErrorBody { error: e.code(), message: e.to_string() })).into_response()
}

// ── request bodies ───────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LineBody {
    item_id: Uuid,
    #[serde(default)]
    revenue_account_id: Option<Uuid>,
    #[serde(default)]
    description: Option<String>,
    quantity: Decimal,
    unit_price: Decimal,
    #[serde(default)]
    line_discount: Decimal,
}
impl From<LineBody> for NewLine {
    fn from(b: LineBody) -> Self {
        NewLine {
            item_id: b.item_id,
            revenue_account_id: b.revenue_account_id,
            description: b.description,
            quantity: b.quantity,
            unit_price: b.unit_price,
            line_discount: b.line_discount,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateQuotationBody {
    quotation_number: String,
    // No `company_id` / `branch_id`: the tenant is derived from the signed token via
    // `CompanyContext`, never from the request body — a client must not be able to name the tenant
    // it writes into.
    customer_id: Uuid,
    quotation_date: chrono::NaiveDate,
    #[serde(default)]
    valid_until: Option<chrono::NaiveDate>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    tax_rate: Decimal,
    #[serde(default)]
    notes: Option<String>,
    lines: Vec<LineBody>,
}
async fn create_quotation(
    State(svc): State<Arc<SellingWriteService>>,
    tenant: CompanyContext,
    Json(b): Json<CreateQuotationBody>,
) -> axum::response::Response {
    let q = NewQuotation {
        quotation_number: b.quotation_number,
        company_id: tenant.company_id,
        branch_id: tenant.branch_id,
        customer_id: b.customer_id,
        quotation_date: b.quotation_date,
        valid_until: b.valid_until,
        currency: b.currency,
        tax_rate: b.tax_rate,
        notes: b.notes,
        lines: b.lines.into_iter().map(Into::into).collect(),
    };
    match svc.create_quotation(q).await {
        Ok(id) => (StatusCode::CREATED, Json(IdResponse { id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSalesOrderBody {
    order_number: String,
    #[serde(default)]
    quotation_id: Option<Uuid>,
    // Tenant comes from the signed token (`CompanyContext`), not the body.
    customer_id: Uuid,
    order_date: chrono::NaiveDate,
    #[serde(default)]
    delivery_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    tax_rate: Decimal,
    #[serde(default)]
    notes: Option<String>,
    lines: Vec<LineBody>,
}
async fn create_sales_order(
    State(svc): State<Arc<SellingWriteService>>,
    tenant: CompanyContext,
    Json(b): Json<CreateSalesOrderBody>,
) -> axum::response::Response {
    let o = NewSalesOrder {
        order_number: b.order_number,
        quotation_id: b.quotation_id,
        company_id: tenant.company_id,
        branch_id: tenant.branch_id,
        customer_id: b.customer_id,
        order_date: b.order_date,
        delivery_date: b.delivery_date,
        currency: b.currency,
        tax_rate: b.tax_rate,
        notes: b.notes,
        lines: b.lines.into_iter().map(Into::into).collect(),
    };
    match svc.create_sales_order(o).await {
        Ok(id) => (StatusCode::CREATED, Json(IdResponse { id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmOrderBody {
    order_id: Uuid,
}
async fn confirm_sales_order(
    State(svc): State<Arc<SellingWriteService>>,
    tenant: CompanyContext,
    Json(b): Json<ConfirmOrderBody>,
) -> axum::response::Response {
    // The tenant scopes the lookup: authentication alone would let a principal of company A confirm
    // company B's order by id, firing B's downstream billing and GL posting.
    match svc.confirm_sales_order(b.order_id, tenant.company_id).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.order_id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSalesInvoiceBody {
    invoice_number: String,
    #[serde(default)]
    sales_order_id: Option<Uuid>,
    // Tenant comes from the signed token (`CompanyContext`), not the body.
    customer_id: Uuid,
    invoice_date: chrono::NaiveDate,
    #[serde(default)]
    due_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    tax_rate: Decimal,
    receivable_account_id: Uuid,
    #[serde(default)]
    tax_output_account_id: Option<Uuid>,
    #[serde(default)]
    notes: Option<String>,
    lines: Vec<LineBody>,
}
async fn create_sales_invoice(
    State(svc): State<Arc<SellingWriteService>>,
    tenant: CompanyContext,
    Json(b): Json<CreateSalesInvoiceBody>,
) -> axum::response::Response {
    let inv = NewSalesInvoice {
        invoice_number: b.invoice_number,
        sales_order_id: b.sales_order_id,
        company_id: tenant.company_id,
        branch_id: tenant.branch_id,
        customer_id: b.customer_id,
        invoice_date: b.invoice_date,
        due_date: b.due_date,
        currency: b.currency,
        tax_rate: b.tax_rate,
        receivable_account_id: b.receivable_account_id,
        tax_output_account_id: b.tax_output_account_id,
        notes: b.notes,
        lines: b.lines.into_iter().map(Into::into).collect(),
    };
    match svc.create_sales_invoice(inv).await {
        Ok(id) => (StatusCode::CREATED, Json(IdResponse { id })).into_response(),
        Err(e) => err_response(e),
    }
}

fn create_selling_write_routes(svc: Arc<SellingWriteService>, verifier: CompanyVerifier) -> Router {
    Router::new()
        .route("/quotations", post(create_quotation))
        .route("/sales-orders", post(create_sales_order))
        .route("/sales-orders/confirm", post(confirm_sales_order))
        .route("/sales-invoices", post(create_sales_invoice))
        // Every write above is tenant-scoped: `company_auth` rejects a request whose token is absent,
        // invalid, or carries no `company_id`, so a handler only ever runs with a proven tenant.
        //
        // `route_layer`, not `layer`: `layer` would also wrap this router's fallback, so once merged
        // every *unmatched* path (e.g. the generic CRUD paths this surface deliberately does not
        // mount) would answer 401 instead of 404 — leaking "auth required" for routes that do not
        // exist, and masking the CRUD-bypass probes.
        .route_layer(from_fn_with_state(verifier, company_auth))
        .with_state(svc)
}

/// Mount the selling module: read all documents + validated, tenant-scoped creates. Generic mutation
/// is not mounted. **Prefer this over `SellingModule::all_crud_routes()` for any real deployment.**
///
/// The composing service builds one [`CompanyVerifier`] from its JWT secret and passes it here; the
/// write surface derives `company_id` from the token, so no tenant crosses the wire in a body.
pub fn create_guarded_selling_routes(
    m: &SellingModule,
    pool: PgPool,
    verifier: CompanyVerifier,
) -> Router {
    let write = Arc::new(SellingWriteService::new(pool));
    Router::new()
        .merge(create_quotation_read_routes(m.quotation_service.clone()))
        .merge(create_quotation_item_read_routes(m.quotation_item_service.clone()))
        .merge(create_sales_order_read_routes(m.sales_order_service.clone()))
        .merge(create_sales_order_item_read_routes(m.sales_order_item_service.clone()))
        .merge(create_sales_invoice_read_routes(m.sales_invoice_service.clone()))
        .merge(create_sales_invoice_item_read_routes(m.sales_invoice_item_service.clone()))
        .merge(create_selling_write_routes(write, verifier))
}
