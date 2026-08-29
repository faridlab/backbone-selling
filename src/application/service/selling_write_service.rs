//! Validated write path for selling (hand-authored, user-owned).
//!
//! Closes the CRUD-bypass: quotations/orders are transactional documents whose money must be
//! internally consistent. The generic 12-endpoint CRUD would let a caller write an order with a
//! mismatched `total` or no lines. Here the create paths compute line amounts + document totals
//! server-side (2dp, round-half-up) and reject an empty document; header+lines are written in ONE
//! transaction. (Selling exited the invoice business — ADR-006; the AR invoice + revenue post are
//! backbone-billing's, reached through the invoice seam.)
//!
//! **This file is the hub:** it holds the module's vocabulary (input structs, outcomes, errors),
//! the money helpers, the document-pricing helper, the repository bag, and the constructors. The
//! write surface is chunked into focused siblings, each an `impl SellingWriteService` block over
//! these same types:
//!
//! - [`super::selling_quotation`] — `create_quotation` (+ template stamping), `accept_quotation`,
//!   and the quotation state machine (send / reject / cancel / re-draft).
//! - [`super::selling_order`] — `create_sales_order`, `create_sales_order_priced` (the promo CART
//!   seam, ADR-002), `confirm_sales_order` (with the confirm-time unit-cost stamp through the
//!   `UnitCostPort`), `convert_quotation_to_order`, `sales_order_ref`, `cancel_sales_order`,
//!   `update_order_line` (the order lock).
//! - [`super::selling_invoice_policy`] — the invoicing-policy engine: the pure per-line
//!   `qty_to_invoice` / `invoice_status` compute and the two invoice-status read models.
//! - [`super::selling_margin`] — the margin engine: the pure per-line margin computes and the
//!   order margin read model over the confirm-time unit-cost snapshots.
//! - [`super::selling_carrier`] — the delivery-carrier registry (create/update/list) and the
//!   order's carrier/tracking metadata verb.
//! - [`super::selling_reinvoice`] — the expense-reinvoice link verbs (attach / list /
//!   mark-invoiced) the host billing adapter drives.
//! - [`super::selling_delivery_seam`] — `build_delivery_request`, `mark_delivered`.
//! - [`super::selling_invoice_seam`] — `build_invoice_request`, `mark_invoiced` (order-to-cash
//!   mirror; capacity-checked `FOR UPDATE` allocation rejects `OverBilled`).
//!
//! Money: `NUMERIC` in the DB; `Decimal` here; half-up to 2dp so `Σ credit == debit` exactly.

use rust_decimal::{Decimal, RoundingStrategy};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use crate::domain::entity::InvoicePolicy;
use crate::infrastructure::persistence::{
    DeliveryCarrierRepository, ExpenseReinvoiceLinkRepository, QuotationItemRepository,
    QuotationRepository, QuotationTemplateRepository, SalesOrderItemRepository,
    SalesOrderRepository,
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
    /// When this line's quantity becomes invoiceable (`None` ⇒ `order`). Carried onto the order
    /// line at conversion; consumed by the policy engine + the billing watermark bound.
    pub invoice_policy: Option<InvoicePolicy>,
    /// Downpayment advance line (`None` ⇒ `false`): stays on the quantity basis for billing but is
    /// excluded from the order invoice-status aggregation.
    pub is_downpayment: Option<bool>,
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
    /// Deal's opportunity this quotation came from (logical link; the host passes it).
    pub opportunity_id: Option<Uuid>,
    /// Template whose validity window + default notes stamp this quotation when the caller
    /// supplied none. Create-time input only — the template itself is not persisted on the quote.
    pub template_id: Option<Uuid>,
    pub lines: Vec<NewLine>,
}

#[derive(Debug, Clone)]
pub struct NewSalesOrder {
    pub order_number: String,
    pub quotation_id: Option<Uuid>,
    /// Carrier chosen at create time (validated against the company's carrier registry before
    /// the insert; the tracking number is verb-only — `set_order_delivery`).
    pub delivery_carrier_id: Option<Uuid>,
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

// (NewSalesInvoice + PostOutcome removed — selling exited the invoice business; ADR-006.)

// --- errors ------------------------------------------------------------------

#[derive(Debug)]
pub enum SellingError {
    EmptyDocument,
    NegativeQuantity,
    DuplicateNumber(String),
    QuotationNotFound(Uuid),
    QuotationNotAccepted(Uuid),
    OrderNotFound(Uuid),
    NotDraft(String),
    OverBilled,
    /// An inbound delivery (`mark_delivered`) tried to advance `delivered_qty` past a line's
    /// `quantity`. Mirror of `OverBilled` for the delivery watermark (council 2026-07-27): without
    /// it the rollup's `delivered_qty >= quantity` silently masks an over-delivery as the delivered
    /// band, and `completed` can become true for stock that was never ordered.
    OverDelivered,
    /// A state-machine verb was called from a state its transition does not allow. The message
    /// names the verb and the current state so the refusal is loud, not a silent no-op.
    InvalidTransition { verb: String, current: String },
    /// Cancelling a quotation that an order was already derived from. A confirmed order must never
    /// be orphaned by resetting its source quotation.
    QuotationOrdered(Uuid),
    /// Cancelling an order with a billed line. Posted invoices are never cancelled — credit notes
    /// are the correction path.
    OrderBilled,
    /// Mutating a frozen field (item/qty/price/discount) of an order line whose order is no longer
    /// a draft. The order lock: confirmed demand is not silently re-priced.
    OrderLineFrozen,
    TemplateNotFound(Uuid),
    TemplateDuplicate(String),
    PricingRejected { code: String, message: String },
    /// The unit-cost source refused the confirm (transport failure, a missing item, or a
    /// negative cost). A confirm is a commitment — an unknown-cost confirm corrupts margin
    /// analytics silently, so the order stays draft. Carries the port's own `code` verbatim
    /// so the host can distinguish and retry.
    CostRejected { code: String, message: String },
    /// The stock engine refused the confirm-time rule launch (transport failure, no route
    /// covers a line's demand). Same fail-closed posture as `CostRejected`: a confirmed
    /// order whose fulfillment silently never launched is corrupt, so the order stays
    /// draft and the launch can be retried with a healthy engine. Carries the port's own
    /// `code` verbatim.
    FulfillmentRejected { code: String, message: String },
    /// The product-surface reader refused the confirm-time service-tracking resolution
    /// (transport failure, unreadable projection). Same fail-closed posture as
    /// `CostRejected`: without the policy the confirm cannot know which lines commit
    /// delivery work, so the order stays draft and the resolution can be retried with a
    /// healthy source. Carries the port's own `code` verbatim. (A product merely MISSING
    /// from the resolution is NOT this error — absence is the manual policy.)
    ServiceCatalogRejected { code: String, message: String },
    /// The project side refused the confirm-time service-delivery mint (transport failure,
    /// a product whose fixed-project anchor is missing). Same fail-closed posture as
    /// `FulfillmentRejected`: a confirmed service order whose delivery work silently never
    /// minted is corrupt, so the order stays draft and the mint can be retried. Carries the
    /// port's own `code` verbatim.
    ServiceDeliveryRejected { code: String, message: String },
    /// The upstream decrease-quantity activity log FAILED after the cancellation itself
    /// had already committed (the port is outside selling's transaction, so the log cannot
    /// roll the flip back). The order IS cancelled; this error says the stock side was not
    /// told — re-invoke the retry verb with a healthy engine. Never silently swallowed.
    DecreaseActivityFailed { code: String, message: String },
    /// Unknown or cross-tenant delivery-carrier id (create-with-carrier / set-delivery /
    /// carrier update). Never surfaced via the FK violation's 500 — a company-scoped pre-read
    /// classifies it.
    CarrierNotFound(Uuid),
    /// A live carrier of this name already exists for the company (mirrors `TemplateDuplicate`).
    CarrierDuplicate(String),
    /// Unknown, wrong-tenant, or soft-deleted expense-reinvoice link.
    ReinvoiceNotFound(Uuid),
    /// This expense is already rebilled on this order (a live link exists — the double-bill guard).
    DuplicateReinvoice,
    /// The rebill amount must be positive (0 or negative refused).
    InvalidReinvoiceAmount,
    Db(sqlx::Error),
    Outbox(String),
}

impl SellingError {
    pub fn code(&self) -> String {
        match self {
            SellingError::EmptyDocument => "empty_document".into(),
            SellingError::NegativeQuantity => "negative_quantity".into(),
            SellingError::DuplicateNumber(_) => "duplicate_number".into(),
            SellingError::QuotationNotFound(_) => "quotation_not_found".into(),
            SellingError::QuotationNotAccepted(_) => "quotation_not_accepted".into(),
            SellingError::OrderNotFound(_) => "order_not_found".into(),
            SellingError::NotDraft(_) => "not_draft".into(),
            SellingError::OverBilled => "over_billed".into(),
            SellingError::OverDelivered => "over_delivered".into(),
            SellingError::InvalidTransition { .. } => "invalid_transition".into(),
            SellingError::QuotationOrdered(_) => "quotation_ordered".into(),
            SellingError::OrderBilled => "order_billed".into(),
            SellingError::OrderLineFrozen => "order_line_frozen".into(),
            SellingError::TemplateNotFound(_) => "template_not_found".into(),
            SellingError::TemplateDuplicate(_) => "duplicate_template_name".into(),
            SellingError::PricingRejected { code, .. } => code.clone(),
            SellingError::CostRejected { code, .. } => code.clone(),
            SellingError::FulfillmentRejected { code, .. } => code.clone(),
            SellingError::ServiceCatalogRejected { code, .. } => code.clone(),
            SellingError::ServiceDeliveryRejected { code, .. } => code.clone(),
            SellingError::DecreaseActivityFailed { code, .. } => code.clone(),
            SellingError::CarrierNotFound(_) => "carrier_not_found".into(),
            SellingError::CarrierDuplicate(_) => "duplicate_carrier_name".into(),
            SellingError::ReinvoiceNotFound(_) => "reinvoice_not_found".into(),
            SellingError::DuplicateReinvoice => "duplicate_reinvoice".into(),
            SellingError::InvalidReinvoiceAmount => "invalid_reinvoice_amount".into(),
            SellingError::Db(_) => "internal_error".into(),
            SellingError::Outbox(_) => "outbox_error".into(),
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            SellingError::QuotationNotFound(_)
            | SellingError::OrderNotFound(_)
            | SellingError::CarrierNotFound(_)
            | SellingError::ReinvoiceNotFound(_) => 404,
            SellingError::Db(_) | SellingError::Outbox(_) => 500,
            _ => 422,
        }
    }
}

impl std::fmt::Display for SellingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SellingError::InvalidTransition { verb, current } => write!(
                f,
                "{verb}: transition not allowed from current state '{current}'"
            ),
            SellingError::QuotationOrdered(_) => write!(
                f,
                "an order was derived from this quotation; it cannot be cancelled"
            ),
            SellingError::OrderBilled => write!(
                f,
                "posted invoices are never cancelled: this order has a billed line"
            ),
            SellingError::OrderLineFrozen => write!(
                f,
                "order is confirmed: only the description may change on its lines"
            ),
            SellingError::TemplateDuplicate(name) => {
                write!(f, "a quotation template named '{name}' already exists")
            }
            SellingError::CostRejected { message, .. } => {
                write!(f, "cost source rejected the confirm: {message}")
            }
            SellingError::FulfillmentRejected { message, .. } => {
                write!(f, "stock engine rejected the confirm-time rule launch: {message}")
            }
            SellingError::ServiceCatalogRejected { message, .. } => {
                write!(f, "product surface rejected the service-tracking resolution: {message}")
            }
            SellingError::ServiceDeliveryRejected { message, .. } => {
                write!(f, "project engine rejected the confirm-time delivery mint: {message}")
            }
            SellingError::DecreaseActivityFailed { message, .. } => {
                write!(
                    f,
                    "the cancellation committed but the upstream decrease-quantity log failed: {message}"
                )
            }
            SellingError::CarrierDuplicate(name) => {
                write!(f, "a delivery carrier named '{name}' already exists")
            }
            SellingError::DuplicateReinvoice => {
                write!(f, "this expense is already rebilled on this order")
            }
            SellingError::InvalidReinvoiceAmount => {
                write!(f, "the rebill amount must be greater than zero")
            }
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
    pub(super) description: Option<String>,
    pub(super) quantity: Decimal,
    pub(super) unit_price: Decimal,
    pub(super) line_discount: Decimal,
    pub(super) line_amount: Decimal,
    pub(super) invoice_policy: InvoicePolicy,
    pub(super) is_downpayment: bool,
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
            description: l.description.clone(),
            quantity: l.quantity,
            unit_price: l.unit_price,
            line_discount: money(l.line_discount),
            line_amount,
            invoice_policy: l.invoice_policy.unwrap_or(InvoicePolicy::Order),
            is_downpayment: l.is_downpayment.unwrap_or(false),
        });
    }
    let subtotal = money(subtotal);
    let tax_amount = money(subtotal * tax_rate / Decimal::from(100));
    let total = subtotal + tax_amount;
    Ok((priced, subtotal, tax_amount, total))
}

/// The repositories this service orchestrates (quotations + orders, each with line children,
/// the quotation-template master, the delivery-carrier registry, and the expense-reinvoice
/// links). Held behind `Arc` so the service stays `Clone` (the repositories are not `Clone`
/// themselves) without re-building them per call. (SalesInvoice repositories lived here before
/// selling exited the invoice business — ADR-006.)
#[derive(Clone)]
pub(super) struct SellingRepos {
    pub(super) quotations: Arc<QuotationRepository>,
    pub(super) quotation_items: Arc<QuotationItemRepository>,
    pub(super) templates: Arc<QuotationTemplateRepository>,
    pub(super) orders: Arc<SalesOrderRepository>,
    pub(super) order_items: Arc<SalesOrderItemRepository>,
    pub(super) carriers: Arc<DeliveryCarrierRepository>,
    pub(super) reinvoices: Arc<ExpenseReinvoiceLinkRepository>,
}

impl SellingRepos {
    fn new(pool: &PgPool) -> Self {
        Self {
            quotations: Arc::new(QuotationRepository::new(pool.clone())),
            quotation_items: Arc::new(QuotationItemRepository::new(pool.clone())),
            templates: Arc::new(QuotationTemplateRepository::new(pool.clone())),
            orders: Arc::new(SalesOrderRepository::new(pool.clone())),
            order_items: Arc::new(SalesOrderItemRepository::new(pool.clone())),
            carriers: Arc::new(DeliveryCarrierRepository::new(pool.clone())),
            reinvoices: Arc::new(ExpenseReinvoiceLinkRepository::new(pool.clone())),
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

    /// Recompute an order's status from its two watermarks (ADR-003): `completed` iff every line is
    /// fully billed AND fully delivered; else `to_deliver` (billed, awaiting delivery) / `to_bill`
    /// (delivered, awaiting billing) / `to_deliver_and_bill` (awaiting both). Never touches a
    /// draft/closed/cancelled order.
    ///
    /// `pub(super)` — the single watermark → status rollup shared by the delivery seam
    /// (`mark_delivered`) and the invoice seam (`mark_invoiced`), so the two watermark advancers can
    /// never drift. (Relocated here from the retired `selling_invoice_post.rs` when selling exited
    /// the invoice business — ADR-006.)
    pub(super) async fn recompute_order_status(&self, order_id: Uuid) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — inherits the caller's scope.
        let row = self.repos.order_items.watermark_rollup(&self.db_pool, order_id).await?;
        let next = match (row.billed_all.unwrap_or(false), row.delivered_all.unwrap_or(false)) {
            (true, true) => "completed",
            (true, false) => "to_deliver",
            (false, true) => "to_bill",
            (false, false) => "to_deliver_and_bill",
        };
        // Only advance an in-flight (confirmed) order; leave draft/closed/cancelled alone — the
        // repo statement's `status = ANY(...)` gate enforces that.
        self.repos.orders.advance_status(&self.db_pool, order_id, next).await?;
        Ok(())
    }
}
