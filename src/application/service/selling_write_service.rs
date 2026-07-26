//! Validated write path for selling (hand-authored, user-owned).
//!
//! Closes the CRUD-bypass: quotations/orders/invoices are transactional documents whose money
//! must be internally consistent and whose GL post must balance. The generic 12-endpoint CRUD
//! would let a caller write an invoice with mismatched `total`, no lines, or post it twice. Here
//! the create paths compute line amounts + document totals server-side (2dp, round-half-up) and
//! reject an empty document; header+lines are written in ONE transaction; `post_sales_invoice`
//! builds a balanced revenue `AccountingPostEnvelope` (Dr A/R · Cr Revenue[per income account]
//! · Cr PPN Output), emits it through the `GlPostSink`, and reconciles the invoice from the ack
//! — idempotently.
//!
//! **This file is the hub:** it holds the module's vocabulary (input structs, outcomes, errors),
//! the money helpers, the document-pricing helper, the repository bag, and the constructors. The
//! write surface is chunked into focused siblings, each an `impl SellingWriteService` block over
//! these same types:
//!
//! - [`super::selling_quotation`] — `create_quotation`, `accept_quotation` (the quotation
//!   lifecycle: draft → accepted).
//! - [`super::selling_order`] — `create_sales_order`, `create_sales_order_priced` (the promo CART
//!   seam, ADR-002), `confirm_sales_order`, `convert_quotation_to_order`, `sales_order_ref`.
//! - [`super::selling_invoice_create`] — `create_sales_invoice`, `create_invoice_from_order`.
//! - [`super::selling_invoice_post`] — `build_revenue_post`, `post_sales_invoice`; owns the
//!   billing-watermark advance and the shared `pub(super) recompute_order_status` used by the
//!   delivery/invoice seams.
//! - [`super::selling_delivery_seam`] — `build_delivery_request`, `mark_delivered`.
//! - [`super::selling_invoice_seam`] — `build_invoice_request`, `mark_invoiced` (order-to-cash
//!   mirror; capacity-checked `FOR UPDATE` allocation rejects `OverBilled`).
//!
//! Money: `NUMERIC` in the DB; `Decimal` here; half-up to 2dp so `Σ credit == debit` exactly.

use rust_decimal::{Decimal, RoundingStrategy};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::infrastructure::persistence::{
    QuotationItemRepository, QuotationRepository, SalesInvoiceItemRepository,
    SalesInvoiceRepository, SalesOrderItemRepository, SalesOrderRepository,
};

use super::selling_events::{LoggingSink, SellingEventSink};

/// Round to 2 decimal places, half away from zero (IDR money convention).
pub(super) fn money(v: Decimal) -> Decimal {
    v.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
}

// --- input structs -----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NewLine {
    pub item_id: Uuid,
    /// Income account for this line (required for invoice lines; ignored for quotation/order).
    pub revenue_account_id: Option<Uuid>,
    pub description: Option<String>,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub line_discount: Decimal,
}

#[derive(Debug, Clone)]
pub struct NewQuotation {
    pub quotation_number: String,
    pub company_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub customer_id: Uuid,
    pub quotation_date: chrono::NaiveDate,
    pub valid_until: Option<chrono::NaiveDate>,
    pub currency: Option<String>,
    pub tax_rate: Decimal,
    pub notes: Option<String>,
    pub lines: Vec<NewLine>,
}

#[derive(Debug, Clone)]
pub struct NewSalesOrder {
    pub order_number: String,
    pub quotation_id: Option<Uuid>,
    pub company_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub customer_id: Uuid,
    pub order_date: chrono::NaiveDate,
    pub delivery_date: Option<chrono::NaiveDate>,
    pub currency: Option<String>,
    pub tax_rate: Decimal,
    pub notes: Option<String>,
    pub lines: Vec<NewLine>,
}

/// One order line to be priced by the cart pricer — carries list price + the dimensions promo matches
/// rules/bundles on (item group, brand), which a plain `NewLine` does not.
#[derive(Debug, Clone)]
pub struct CartOrderLine {
    pub item_id: Uuid,
    pub item_group_id: Option<Uuid>,
    pub brand_id: Option<Uuid>,
    pub revenue_account_id: Option<Uuid>,
    pub description: Option<String>,
    pub list_price: Decimal,
    pub quantity: Decimal,
}

/// A Sales Order priced through the promo cart seam (`create_sales_order_priced`).
#[derive(Debug, Clone)]
pub struct NewCartSalesOrder {
    pub order_number: String,
    pub company_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub customer_id: Uuid,
    pub customer_group_id: Option<Uuid>,
    pub coupon_code: Option<String>,
    pub order_date: chrono::NaiveDate,
    pub delivery_date: Option<chrono::NaiveDate>,
    pub currency: Option<String>,
    pub tax_rate: Decimal,
    pub notes: Option<String>,
    pub lines: Vec<CartOrderLine>,
}

#[derive(Debug, Clone)]
pub struct NewSalesInvoice {
    pub invoice_number: String,
    pub sales_order_id: Option<Uuid>,
    pub company_id: Uuid,
    pub branch_id: Option<Uuid>,
    pub customer_id: Uuid,
    pub invoice_date: chrono::NaiveDate,
    pub due_date: Option<chrono::NaiveDate>,
    pub currency: Option<String>,
    pub tax_rate: Decimal,
    /// A/R control account to debit (the "debit_to").
    pub receivable_account_id: Uuid,
    /// PPN Output account — required iff the computed tax is > 0.
    pub tax_output_account_id: Option<Uuid>,
    pub notes: Option<String>,
    pub lines: Vec<NewLine>,
}

/// Outcome of posting an invoice to the GL.
#[derive(Debug, Clone)]
pub struct PostOutcome {
    pub invoice_id: Uuid,
    pub post_id: Uuid,
    pub journal_id: Uuid,
    /// True when the invoice was already posted (idempotent replay — no new emission).
    pub idempotent_reuse: bool,
}

// --- errors ------------------------------------------------------------------

#[derive(Debug)]
pub enum SellingError {
    EmptyDocument,
    NegativeQuantity,
    MissingRevenueAccount,
    TaxAccountMissing,
    UnbalancedPost,
    UnsupportedCurrency(String),
    DuplicateNumber(String),
    InvoiceNotFound(Uuid),
    QuotationNotFound(Uuid),
    QuotationNotAccepted(Uuid),
    OrderNotFound(Uuid),
    NotDraft(String),
    OverBilled,
    GlRejected { code: String, message: String },
    PricingRejected { code: String, message: String },
    Db(sqlx::Error),
    Outbox(String),
}

impl SellingError {
    pub fn code(&self) -> String {
        match self {
            SellingError::EmptyDocument => "empty_document".into(),
            SellingError::NegativeQuantity => "negative_quantity".into(),
            SellingError::MissingRevenueAccount => "missing_revenue_account".into(),
            SellingError::TaxAccountMissing => "tax_account_missing".into(),
            SellingError::UnbalancedPost => "unbalanced_post".into(),
            SellingError::UnsupportedCurrency(_) => "unsupported_currency".into(),
            SellingError::DuplicateNumber(_) => "duplicate_number".into(),
            SellingError::InvoiceNotFound(_) => "invoice_not_found".into(),
            SellingError::QuotationNotFound(_) => "quotation_not_found".into(),
            SellingError::QuotationNotAccepted(_) => "quotation_not_accepted".into(),
            SellingError::OrderNotFound(_) => "order_not_found".into(),
            SellingError::NotDraft(_) => "not_draft".into(),
            SellingError::OverBilled => "over_billed".into(),
            // Surface the GL's own stable code so callers see one contract vocabulary.
            SellingError::GlRejected { code, .. } => code.clone(),
            SellingError::PricingRejected { code, .. } => code.clone(),
            SellingError::Db(_) => "internal_error".into(),
            SellingError::Outbox(_) => "outbox_error".into(),
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            SellingError::InvoiceNotFound(_)
            | SellingError::QuotationNotFound(_)
            | SellingError::OrderNotFound(_) => 404,
            SellingError::Db(_) | SellingError::Outbox(_) => 500,
            _ => 422,
        }
    }
}

impl std::fmt::Display for SellingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SellingError::GlRejected { code, message } => write!(f, "{code}: {message}"),
            other => write!(f, "{}", other.code()),
        }
    }
}
impl std::error::Error for SellingError {}
impl From<sqlx::Error> for SellingError {
    fn from(e: sqlx::Error) -> Self {
        SellingError::Db(e)
    }
}

pub(super) fn is_dup(e: &sqlx::Error) -> bool {
    e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false)
}

/// A priced line after server-side computation.
pub(super) struct PricedLine {
    pub(super) item_id: Uuid,
    pub(super) revenue_account_id: Option<Uuid>,
    pub(super) description: Option<String>,
    pub(super) quantity: Decimal,
    pub(super) unit_price: Decimal,
    pub(super) line_discount: Decimal,
    pub(super) line_amount: Decimal,
}

/// Compute `line_amount = money(qty*price) - discount` per line and the document totals
/// `(subtotal, tax_amount, total)`. Rejects empty/negative documents.
pub(super) fn price_document(lines: &[NewLine], tax_rate: Decimal) -> Result<(Vec<PricedLine>, Decimal, Decimal, Decimal), SellingError> {
    if lines.is_empty() {
        return Err(SellingError::EmptyDocument);
    }
    let mut priced = Vec::with_capacity(lines.len());
    let mut subtotal = Decimal::ZERO;
    for l in lines {
        if l.quantity < Decimal::ZERO || l.unit_price < Decimal::ZERO || l.line_discount < Decimal::ZERO {
            return Err(SellingError::NegativeQuantity);
        }
        let gross = money(l.quantity * l.unit_price);
        let line_amount = gross - money(l.line_discount);
        if line_amount < Decimal::ZERO {
            return Err(SellingError::NegativeQuantity);
        }
        subtotal += line_amount;
        priced.push(PricedLine {
            item_id: l.item_id,
            revenue_account_id: l.revenue_account_id,
            description: l.description.clone(),
            quantity: l.quantity,
            unit_price: l.unit_price,
            line_discount: money(l.line_discount),
            line_amount,
        });
    }
    let subtotal = money(subtotal);
    let tax_amount = money(subtotal * tax_rate / Decimal::from(100));
    let total = subtotal + tax_amount;
    Ok((priced, subtotal, tax_amount, total))
}

/// The six document repositories this service orchestrates. Held behind `Arc` so the service stays
/// `Clone` (the repositories are not `Clone` themselves) without re-building them per call.
#[derive(Clone)]
pub(super) struct SellingRepos {
    pub(super) quotations: Arc<QuotationRepository>,
    pub(super) quotation_items: Arc<QuotationItemRepository>,
    pub(super) orders: Arc<SalesOrderRepository>,
    pub(super) order_items: Arc<SalesOrderItemRepository>,
    pub(super) invoices: Arc<SalesInvoiceRepository>,
    pub(super) invoice_items: Arc<SalesInvoiceItemRepository>,
}

impl SellingRepos {
    fn new(pool: &PgPool) -> Self {
        Self {
            quotations: Arc::new(QuotationRepository::new(pool.clone())),
            quotation_items: Arc::new(QuotationItemRepository::new(pool.clone())),
            orders: Arc::new(SalesOrderRepository::new(pool.clone())),
            order_items: Arc::new(SalesOrderItemRepository::new(pool.clone())),
            invoices: Arc::new(SalesInvoiceRepository::new(pool.clone())),
            invoice_items: Arc::new(SalesInvoiceItemRepository::new(pool.clone())),
        }
    }
}

#[derive(Clone)]
pub struct SellingWriteService {
    pub(super) db_pool: PgPool,
    pub(super) sink: Arc<dyn SellingEventSink>,
    pub(super) repos: SellingRepos,
}

impl SellingWriteService {
    pub fn new(db_pool: PgPool) -> Self {
        let repos = SellingRepos::new(&db_pool);
        Self { db_pool, sink: Arc::new(LoggingSink), repos }
    }

    /// Construct with a custom domain-event sink (a bus adapter, or a test recorder / consumer rule).
    pub fn with_sink(db_pool: PgPool, sink: Arc<dyn SellingEventSink>) -> Self {
        let repos = SellingRepos::new(&db_pool);
        Self { db_pool, sink, repos }
    }
}
