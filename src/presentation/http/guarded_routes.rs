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
//! The confirm route takes the unit-cost port (catalog standard-cost seam) as a REQUIRED argument
//! of [`create_guarded_selling_routes`] — the composing service supplies its catalog-backed
//! implementation; confirm stamps each line's `unit_cost` snapshot through it before the
//! draft → confirmed flip. The generic CRUD read routers for the carrier registry and the
//! expense-reinvoice link are merged unauthenticated (same posture as the document reads); every
//! WRITE below rides `company_auth` and derives its tenant from the signed token.
//!
//! `SellingWriteService` is stateless over the pool, so it is constructed here rather than pulled
//! from the generated `SellingModule` struct — the guarded surface survives a regen of the module.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State}, http::StatusCode, middleware::from_fn_with_state,
    response::IntoResponse, routing::{get, patch, post}, Json, Router,
};
use backbone_auth::company::{company_auth, CompanyContext, CompanyVerifier};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::application::service::selling_carrier::UpdateCarrierPatch;
use crate::application::service::selling_order::UpdateOrderLinePatch;
use crate::application::service::selling_unit_cost::UnitCostPort;
use crate::application::service::selling_write_service::{
    NewLine, NewQuotation, NewSalesOrder, SellingError, SellingWriteService,
};
use crate::domain::entity::InvoicePolicy;
use crate::infrastructure::persistence::QuotationTemplateRow;
use crate::SellingModule;

use super::{
    create_delivery_carrier_read_routes, create_expense_reinvoice_link_read_routes,
    create_quotation_read_routes, create_quotation_item_read_routes,
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

/// Handler state: the write service plus the unit-cost port that confirm passes through. The port
/// is wiring, not service state — the service stays stateless over the pool and takes the port
/// per call, so the composing host keeps full control of the cost source.
#[derive(Clone)]
struct SellingWriteState {
    svc: Arc<SellingWriteService>,
    costs: Arc<dyn UnitCostPort>,
}

/// `Option<Option<T>>` deserialization that distinguishes MISSING (keep the stored value) from
/// EXPLICIT NULL (clear it): `#[serde(default, deserialize_with = "double_option")]` maps an
/// absent field to `None` and a JSON `null` to `Some(None)`.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
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
    /// When this line's quantity becomes invoiceable: on confirmation (`order`, the default) or on
    /// delivery (`delivery`). Optional — omitted lines bill on the ordered quantity.
    #[serde(default)]
    invoice_policy: Option<InvoicePolicy>,
    /// Marks an advance/downpayment line: billed on the ordered quantity, excluded from the order's
    /// aggregate invoice status. Optional — default false.
    #[serde(default)]
    is_downpayment: Option<bool>,
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
            invoice_policy: b.invoice_policy,
            is_downpayment: b.is_downpayment,
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
    /// Stamp this template's validity window + default notes when the caller supplied none. The
    /// template itself is not persisted on the quotation — its effects are stamped at create.
    #[serde(default)]
    template_id: Option<Uuid>,
    /// The deal's opportunity this quotation came from (a logical link the host CRM passes through;
    /// selling takes it on faith — no cross-module key).
    #[serde(default)]
    opportunity_id: Option<Uuid>,
    lines: Vec<LineBody>,
}
async fn create_quotation(
    State(st): State<SellingWriteState>,
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
        template_id: b.template_id,
        opportunity_id: b.opportunity_id,
        lines: b.lines.into_iter().map(Into::into).collect(),
    };
    match st.svc.create_quotation(q).await {
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
    /// Optional create-time delivery carrier — must name one of THIS company's carriers (an
    /// unknown or cross-tenant id refuses `carrier_not_found`, never the FK violation's 500).
    #[serde(default)]
    delivery_carrier_id: Option<Uuid>,
    lines: Vec<LineBody>,
}
async fn create_sales_order(
    State(st): State<SellingWriteState>,
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
        delivery_carrier_id: b.delivery_carrier_id,
        lines: b.lines.into_iter().map(Into::into).collect(),
    };
    match st.svc.create_sales_order(o).await {
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
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<ConfirmOrderBody>,
) -> axum::response::Response {
    // The tenant scopes the lookup: authentication alone would let a principal of company A confirm
    // company B's order by id, firing B's downstream billing and GL posting. The cost port comes
    // from the composition (the host's catalog adapter) — a failing port refuses the confirm with
    // `cost_rejected`, it never confirms with a silently unknown cost.
    match st.svc.confirm_sales_order(b.order_id, tenant.company_id, st.costs.as_ref()).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.order_id })).into_response(),
        Err(e) => err_response(e),
    }
}

// (create_sales_invoice + CreateSalesInvoiceBody removed — selling exited the invoice business;
// the AR invoice is now billing's. ADR-006.)

// ── quotation state machine ──────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotationVerbBody {
    quotation_id: Uuid,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuotationReasonBody {
    quotation_id: Uuid,
    #[serde(default)]
    reason: Option<String>,
}
async fn send_quotation(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<QuotationVerbBody>,
) -> axum::response::Response {
    match st.svc.send_quotation(b.quotation_id, tenant.company_id).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.quotation_id })).into_response(),
        Err(e) => err_response(e),
    }
}
async fn redraft_quotation(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<QuotationVerbBody>,
) -> axum::response::Response {
    match st.svc.redraft_quotation(b.quotation_id, tenant.company_id).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.quotation_id })).into_response(),
        Err(e) => err_response(e),
    }
}
async fn reject_quotation(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<QuotationReasonBody>,
) -> axum::response::Response {
    match st.svc.reject_quotation(b.quotation_id, tenant.company_id, b.reason).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.quotation_id })).into_response(),
        Err(e) => err_response(e),
    }
}
async fn cancel_quotation(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<QuotationReasonBody>,
) -> axum::response::Response {
    match st.svc.cancel_quotation(b.quotation_id, tenant.company_id, b.reason).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.quotation_id })).into_response(),
        Err(e) => err_response(e),
    }
}
async fn accept_quotation(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<QuotationVerbBody>,
) -> axum::response::Response {
    match st.svc.accept_quotation(b.quotation_id, tenant.company_id).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.quotation_id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConvertQuotationBody {
    quotation_id: Uuid,
    order_number: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConvertQuotationResponse {
    order_id: Uuid,
    quotation_id: Uuid,
}
async fn convert_quotation_to_order(
    State(st): State<SellingWriteState>,
    Json(b): Json<ConvertQuotationBody>,
) -> axum::response::Response {
    match st.svc.convert_quotation_to_order(b.quotation_id, b.order_number).await {
        Ok(order_id) => (StatusCode::CREATED, Json(ConvertQuotationResponse {
            order_id,
            quotation_id: b.quotation_id,
        })).into_response(),
        Err(e) => err_response(e),
    }
}

// ── order machine: cancel + line edits ───────────────────────────────────────
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderVerbBody {
    order_id: Uuid,
}
async fn cancel_sales_order(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<OrderVerbBody>,
) -> axum::response::Response {
    match st.svc.cancel_sales_order(b.order_id, tenant.company_id).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.order_id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateOrderLineBody {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    item_id: Option<Uuid>,
    #[serde(default)]
    quantity: Option<Decimal>,
    #[serde(default)]
    unit_price: Option<Decimal>,
    #[serde(default)]
    line_discount: Option<Decimal>,
}
async fn update_order_line(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Path(line_id): Path<Uuid>,
    Json(b): Json<UpdateOrderLineBody>,
) -> axum::response::Response {
    // On a confirmed order only the description is editable — the service refuses item/qty/price/
    // discount with `order_line_frozen` once the status has left `draft`.
    match st.svc.update_order_line(line_id, tenant.company_id, UpdateOrderLinePatch {
        description: b.description,
        item_id: b.item_id,
        quantity: b.quantity,
        unit_price: b.unit_price,
        line_discount: b.line_discount,
    }).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: line_id })).into_response(),
        Err(e) => err_response(e),
    }
}

// ── invoicing-policy read models ─────────────────────────────────────────────
// `qty_to_invoice` / `invoice_status` are computed at read time and never accepted on any write
// body — there is no route that could persist them.
async fn order_invoice_status(
    State(st): State<SellingWriteState>,
    _tenant: CompanyContext,
    Path(order_id): Path<Uuid>,
) -> axum::response::Response {
    match st.svc.order_invoice_view(order_id).await {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(e) => err_response(e),
    }
}
async fn quotation_invoice_status(
    State(st): State<SellingWriteState>,
    _tenant: CompanyContext,
    Path(quotation_id): Path<Uuid>,
) -> axum::response::Response {
    match st.svc.quotation_invoice_view(quotation_id).await {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(e) => err_response(e),
    }
}

// ── margin read model ────────────────────────────────────────────────────────
// Same shape as the invoice-status reads: pure read-time compute over the confirm-time unit-cost
// snapshots. `margin` / `marginPercent` are NOT schema fields and appear on no write body — a
// client cannot inject a cost or a margin through any route; the only writer of `unit_cost` is the
// confirm flow's stamp, fed by the host-supplied cost port.
async fn order_margin_view(
    State(st): State<SellingWriteState>,
    _tenant: CompanyContext,
    Path(order_id): Path<Uuid>,
) -> axum::response::Response {
    match st.svc.order_margin_view(order_id).await {
        Ok(view) => (StatusCode::OK, Json(view)).into_response(),
        Err(e) => err_response(e),
    }
}

// ── quotation templates ──────────────────────────────────────────────────────
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TemplateResponse {
    id: Uuid,
    name: String,
    validity_days: i32,
    default_notes: Option<String>,
}
impl From<QuotationTemplateRow> for TemplateResponse {
    fn from(r: QuotationTemplateRow) -> Self {
        TemplateResponse {
            id: r.id,
            name: r.name,
            validity_days: r.validity_days,
            default_notes: r.default_notes,
        }
    }
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTemplateBody {
    name: String,
    #[serde(default)]
    validity_days: Option<i32>,
    #[serde(default)]
    default_notes: Option<String>,
}
async fn create_quotation_template(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<CreateTemplateBody>,
) -> axum::response::Response {
    // Validity defaults to 30 days when omitted; the write itself is fenced to the caller's
    // tenant and a duplicate name refuses with `duplicate_template_name`.
    match st.svc
        .create_quotation_template(
            tenant.company_id,
            &b.name,
            b.validity_days.unwrap_or(30),
            b.default_notes.as_deref(),
        )
        .await
    {
        Ok(id) => (StatusCode::CREATED, Json(IdResponse { id })).into_response(),
        Err(e) => err_response(e),
    }
}
async fn list_quotation_templates(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
) -> axum::response::Response {
    match st.svc.list_quotation_templates(tenant.company_id).await {
        Ok(rows) => {
            let list: Vec<TemplateResponse> = rows.into_iter().map(Into::into).collect();
            (StatusCode::OK, Json(list)).into_response()
        }
        Err(e) => err_response(e),
    }
}

// ── delivery-carrier registry (master data; REGISTRY ONLY — no rates, no labels) ──
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateCarrierBody {
    name: String,
    /// e.g. `https://track.example.com/{tracking_ref}` — optional at create; absent = none.
    #[serde(default)]
    tracking_url_template: Option<String>,
}
async fn create_carrier(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<CreateCarrierBody>,
) -> axum::response::Response {
    match st.svc
        .create_delivery_carrier(tenant.company_id, &b.name, b.tracking_url_template.as_deref())
        .await
    {
        Ok(id) => (StatusCode::CREATED, Json(IdResponse { id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CarrierListQuery {
    /// `false` includes deactivated carriers (history stays readable); default `true` — the
    /// active set is the pick-list, the retired set is only occasionally interesting.
    #[serde(default)]
    active_only: Option<bool>,
}
async fn list_carriers(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Query(q): Query<CarrierListQuery>,
) -> axum::response::Response {
    match st.svc.list_delivery_carriers(tenant.company_id, q.active_only.unwrap_or(true)).await {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCarrierBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    /// `null` CLEARS the stored template; omitting the field keeps it (`double_option`).
    #[serde(default, deserialize_with = "double_option")]
    tracking_url_template: Option<Option<String>>,
}
async fn update_carrier(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Path(carrier_id): Path<Uuid>,
    Json(b): Json<UpdateCarrierBody>,
) -> axum::response::Response {
    // Retirement is deactivate-don't-delete: `{"active": false}`. A hard delete would break the
    // orders that reference the carrier through the FK.
    match st.svc.update_delivery_carrier(carrier_id, tenant.company_id, UpdateCarrierPatch {
        name: b.name,
        active: b.active,
        tracking_url_template: b.tracking_url_template,
    }).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: carrier_id })).into_response(),
        Err(e) => err_response(e),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDeliveryBody {
    order_id: Uuid,
    /// MISSING keeps the stored carrier; `null` CLEARS it; a value SETS it (and must name one of
    /// this company's carriers — `carrier_not_found`, never an FK 500).
    #[serde(default, deserialize_with = "double_option")]
    delivery_carrier_id: Option<Option<Uuid>>,
    /// MISSING keeps the stored tracking ref; `null` CLEARS it; a value SETS it.
    #[serde(default, deserialize_with = "double_option")]
    tracking_ref: Option<Option<String>>,
}
async fn set_order_delivery(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Json(b): Json<SetDeliveryBody>,
) -> axum::response::Response {
    // Fulfillment metadata, not frozen money: writable on draft AND confirmed orders (tracking
    // typically arrives only after ship), refused on cancelled ones.
    match st.svc
        .set_order_delivery(b.order_id, tenant.company_id, b.delivery_carrier_id, b.tracking_ref)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: b.order_id })).into_response(),
        Err(e) => err_response(e),
    }
}

// ── expense-reinvoice links (the host billing adapter's pull surface) ─────────
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachReinvoiceBody {
    /// The expense to rebill — selling takes this id ON FAITH (no cross-module key): the host
    /// validates the expense exists, belongs to the same company, and is postable before calling.
    expense_id: Uuid,
    /// Rebill amount in the ORDER's currency; must be positive.
    amount: Decimal,
}
async fn attach_expense_reinvoice(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Path(order_id): Path<Uuid>,
    Json(b): Json<AttachReinvoiceBody>,
) -> axum::response::Response {
    match st.svc
        .attach_expense_reinvoice(order_id, b.expense_id, b.amount, tenant.company_id)
        .await
    {
        Ok(id) => (StatusCode::CREATED, Json(IdResponse { id })).into_response(),
        Err(e) => err_response(e),
    }
}
async fn list_expense_reinvoices(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Path(order_id): Path<Uuid>,
) -> axum::response::Response {
    match st.svc.list_expense_reinvoices(order_id, tenant.company_id).await {
        Ok(rows) => (StatusCode::OK, Json(rows)).into_response(),
        Err(e) => err_response(e),
    }
}
async fn mark_expense_reinvoice_invoiced(
    State(st): State<SellingWriteState>,
    tenant: CompanyContext,
    Path(link_id): Path<Uuid>,
) -> axum::response::Response {
    // Called by the host billing adapter after its invoice post acked. A double mark is a LOUD
    // refusal (`invalid_transition`), not a silent success.
    match st.svc.mark_expense_reinvoice_invoiced(link_id, tenant.company_id).await {
        Ok(()) => (StatusCode::OK, Json(IdResponse { id: link_id })).into_response(),
        Err(e) => err_response(e),
    }
}

fn create_selling_write_routes(state: SellingWriteState, verifier: CompanyVerifier) -> Router {
    Router::new()
        .route("/quotations", post(create_quotation))
        // Quotation state machine: send → accept → reject → re-draft round trips, cancel is the exit.
        // Refusals are loud 422s (`invalid_transition` / `quotation_ordered`), never silent no-ops.
        .route("/quotations/send", post(send_quotation))
        .route("/quotations/accept", post(accept_quotation))
        .route("/quotations/re-draft", post(redraft_quotation))
        .route("/quotations/reject", post(reject_quotation))
        .route("/quotations/cancel", post(cancel_quotation))
        .route("/quotations/convert-to-order", post(convert_quotation_to_order))
        .route("/quotations/:id/invoice-status", get(quotation_invoice_status))
        .route("/quotation-templates", post(create_quotation_template).get(list_quotation_templates))
        .route("/sales-orders", post(create_sales_order))
        .route("/sales-orders/confirm", post(confirm_sales_order))
        .route("/sales-orders/cancel", post(cancel_sales_order))
        .route("/sales-orders/lines/:id", patch(update_order_line))
        .route("/sales-orders/:id/invoice-status", get(order_invoice_status))
        .route("/sales-orders/:id/margin", get(order_margin_view))
        .route("/sales-orders/:id/expense-reinvoices",
               get(list_expense_reinvoices).post(attach_expense_reinvoice))
        .route("/expense-reinvoices/:id/mark-invoiced", post(mark_expense_reinvoice_invoiced))
        // Delivery-carrier registry: master + the order link. Deactivate-don't-delete.
        .route("/delivery-carriers", post(create_carrier).get(list_carriers))
        .route("/delivery-carriers/:id", patch(update_carrier))
        .route("/sales-orders/set-delivery", post(set_order_delivery))
        // Every write above is tenant-scoped: `company_auth` rejects a request whose token is absent,
        // invalid, or carries no `company_id`, so a handler only ever runs with a proven tenant.
        //
        // `route_layer`, not `layer`: `layer` would also wrap this router's fallback, so once merged
        // every *unmatched* path (e.g. the generic CRUD paths this surface deliberately does not
        // mount) would answer 401 instead of 404 — leaking "auth required" for routes that do not
        // exist, and masking the CRUD-bypass probes.
        .route_layer(from_fn_with_state(verifier, company_auth))
        .with_state(state)
}

/// Mount the selling module: read all documents + validated, tenant-scoped creates. Generic mutation
/// is not mounted. **Prefer this over `SellingModule::all_crud_routes()` for any real deployment.**
///
/// The composing service builds one [`CompanyVerifier`] from its JWT secret and passes it here; the
/// write surface derives `company_id` from the token, so no tenant crosses the wire in a body.
///
/// `unit_cost` is REQUIRED: the cost source behind the confirm-time margin snapshot (the catalog
/// standard-cost seam). Pass [`crate::application::service::selling_unit_cost::NoUnitCostPort`]
/// only in compositions that never confirm orders — it resolves every cost to NULL, which confirm
/// treats as honest absence (margin reads null, never zero).
pub fn create_guarded_selling_routes(
    m: &SellingModule,
    pool: PgPool,
    verifier: CompanyVerifier,
    unit_cost: Arc<dyn UnitCostPort>,
) -> Router {
    let write = SellingWriteState {
        svc: Arc::new(SellingWriteService::new(pool)),
        costs: unit_cost,
    };
    Router::new()
        .merge(create_quotation_read_routes(m.quotation_service.clone()))
        .merge(create_quotation_item_read_routes(m.quotation_item_service.clone()))
        .merge(create_sales_order_read_routes(m.sales_order_service.clone()))
        .merge(create_sales_order_item_read_routes(m.sales_order_item_service.clone()))
        // Generic reads for the two registry entities, same posture as the document reads above
        // (unauthenticated SELECT-only routers at the snake_case paths; every write on those
        // entities goes exclusively through the validated verbs above).
        .merge(create_delivery_carrier_read_routes(m.delivery_carrier_service.clone()))
        .merge(create_expense_reinvoice_link_read_routes(m.expense_reinvoice_link_service.clone()))
        .merge(create_selling_write_routes(write, verifier))
}
