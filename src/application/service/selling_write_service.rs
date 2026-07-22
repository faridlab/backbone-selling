//! Validated write path for selling (hand-authored, user-owned).
//!
//! Closes the CRUD-bypass: quotations/orders/invoices are transactional documents whose money
//! must be internally consistent and whose GL post must balance. The generic 12-endpoint CRUD
//! would let a caller write an invoice with mismatched `total`, no lines, or post it twice. Here:
//!   - creates compute line amounts + document totals server-side (2dp, round-half-up) and reject
//!     an empty document; header+lines are written in ONE transaction.
//!   - `post_sales_invoice` builds a balanced revenue `AccountingPostEnvelope`
//!     (Dr A/R · Cr Revenue[per income account] · Cr PPN Output), emits it through the
//!     `GlPostSink`, and reconciles the invoice from the ack — idempotently.
//!
//! Money: `NUMERIC` in the DB; `Decimal` here; half-up to 2dp so `Σ credit == debit` exactly.

use backbone_orm::company_scope;
use rust_decimal::{Decimal, RoundingStrategy};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::infrastructure::persistence::{
    NewQuotationItemRow, NewQuotationRow, NewSalesInvoiceItemRow, NewSalesInvoiceRow,
    NewSalesOrderItemRow, NewSalesOrderRow, QuotationItemRepository, QuotationRepository,
    SalesInvoiceItemRepository, SalesInvoiceRepository, SalesOrderItemRepository,
    SalesOrderRepository,
};

use super::selling_events::{
    DeliveryRequestEnvelope, DeliveryRequestLine, InvoiceRequestEnvelope, InvoiceRequestLine,
    QuotationAccepted, SalesInvoiceIssued, SalesInvoicePosted, SalesOrderConfirmed, SalesOrderRef,
    SellingEvent, SellingEventSink, LoggingSink,
};
use super::selling_gl::{AccountingPostEnvelope, GlPostLine, GlPostSink};
use super::selling_cart_pricing::{CartPriceLine, CartPriceRequest, CartPricingPort};

/// Round to 2 decimal places, half away from zero (IDR money convention).
fn money(v: Decimal) -> Decimal {
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

fn is_dup(e: &sqlx::Error) -> bool {
    e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false)
}

/// A priced line after server-side computation.
struct PricedLine {
    item_id: Uuid,
    revenue_account_id: Option<Uuid>,
    description: Option<String>,
    quantity: Decimal,
    unit_price: Decimal,
    line_discount: Decimal,
    line_amount: Decimal,
}

/// Compute `line_amount = money(qty*price) - discount` per line and the document totals
/// `(subtotal, tax_amount, total)`. Rejects empty/negative documents.
fn price_document(lines: &[NewLine], tax_rate: Decimal) -> Result<(Vec<PricedLine>, Decimal, Decimal, Decimal), SellingError> {
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
struct SellingRepos {
    quotations: Arc<QuotationRepository>,
    quotation_items: Arc<QuotationItemRepository>,
    orders: Arc<SalesOrderRepository>,
    order_items: Arc<SalesOrderItemRepository>,
    invoices: Arc<SalesInvoiceRepository>,
    invoice_items: Arc<SalesInvoiceItemRepository>,
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
    db_pool: PgPool,
    sink: Arc<dyn SellingEventSink>,
    repos: SellingRepos,
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

    // ---- Quotation ----------------------------------------------------------

    pub async fn create_quotation(&self, q: NewQuotation) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&q.lines, q.tax_rate)?;
        let id = Uuid::new_v4();
        let currency = q.currency.unwrap_or_else(|| "IDR".into());
        // RLS scope (ADR-0008): the header+lines insert runs in ONE transaction whose connection is
        // bound to this document's company, so every write is fenced by `app.company_id`. The explicit
        // `company_id` binds below stay as defense-in-depth.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, q.company_id).await?;
        let r = self.repos.quotations.insert_draft(&mut tx, &NewQuotationRow {
            id,
            quotation_number: &q.quotation_number,
            company_id: q.company_id,
            branch_id: q.branch_id,
            customer_id: q.customer_id,
            quotation_date: q.quotation_date,
            valid_until: q.valid_until,
            currency: &currency,
            subtotal,
            tax_rate: q.tax_rate,
            tax_amount,
            total,
            notes: q.notes.as_deref(),
        }).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(q.quotation_number) } else { e.into() });
        }
        for p in &priced {
            self.repos.quotation_items.insert_line(&mut tx, &NewQuotationItemRow {
                id: Uuid::new_v4(),
                quotation_id: id,
                company_id: q.company_id,
                item_id: p.item_id,
                description: p.description.as_deref(),
                quantity: p.quantity,
                unit_price: p.unit_price,
                line_discount: p.line_discount,
                line_amount: p.line_amount,
            }).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    // ---- Sales Order --------------------------------------------------------

    pub async fn create_sales_order(&self, o: NewSalesOrder) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&o.lines, o.tax_rate)?;
        let id = Uuid::new_v4();
        let currency = o.currency.unwrap_or_else(|| "IDR".into());
        // RLS scope (ADR-0008): bind the order's company onto the header+lines transaction.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, o.company_id).await?;
        let r = self.repos.orders.insert_draft(&mut tx, &NewSalesOrderRow {
            id,
            order_number: &o.order_number,
            quotation_id: o.quotation_id,
            company_id: o.company_id,
            branch_id: o.branch_id,
            customer_id: o.customer_id,
            order_date: o.order_date,
            delivery_date: o.delivery_date,
            currency: &currency,
            subtotal,
            tax_rate: o.tax_rate,
            tax_amount,
            total,
            notes: o.notes.as_deref(),
        }).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(o.order_number) } else { e.into() });
        }
        for p in &priced {
            self.repos.order_items.insert_line(&mut tx, &NewSalesOrderItemRow {
                id: Uuid::new_v4(),
                order_id: id,
                company_id: o.company_id,
                item_id: p.item_id,
                description: p.description.as_deref(),
                quantity: p.quantity,
                unit_price: p.unit_price,
                line_discount: p.line_discount,
                line_amount: p.line_amount,
            }).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Create a Sales Order whose prices are resolved by promo's CART pricer (the cart seam, ADR-002).
    /// Selling passes the whole basket (list prices + item dimensions + optional coupon) to the
    /// `CartPricingPort`; promo returns per-line nets that already fold in line rules, order-total
    /// discounts, and bundles. Selling maps each net back to a `unit_price`/`line_discount` pair so the
    /// order's own `price_document` reproduces the cart total exactly. Zero normal Cargo edge to promo.
    pub async fn create_sales_order_priced(
        &self,
        o: NewCartSalesOrder,
        pricing: &dyn CartPricingPort,
    ) -> Result<Uuid, SellingError> {
        if o.lines.is_empty() {
            return Err(SellingError::EmptyDocument);
        }
        // Build the pricing request, keeping a parallel line_ref → input-index map.
        let refs: Vec<Uuid> = o.lines.iter().map(|_| Uuid::new_v4()).collect();
        let req = CartPriceRequest {
            company_id: o.company_id,
            customer_id: Some(o.customer_id),
            customer_group_id: o.customer_group_id,
            coupon_code: o.coupon_code.clone(),
            lines: o
                .lines
                .iter()
                .zip(&refs)
                .map(|(l, r)| CartPriceLine {
                    line_ref: *r,
                    item_id: l.item_id,
                    item_group_id: l.item_group_id,
                    brand_id: l.brand_id,
                    list_price: l.list_price,
                    quantity: l.quantity,
                })
                .collect(),
        };
        let priced = pricing
            .price_cart(&req)
            .await
            .map_err(|e| SellingError::PricingRejected { code: e.code, message: e.message })?;

        // Map each priced net back to (unit_price, line_discount) so line_amount == net_line_total.
        let mut lines = Vec::with_capacity(o.lines.len());
        for (l, r) in o.lines.iter().zip(&refs) {
            let pl = priced
                .lines
                .iter()
                .find(|p| p.line_ref == *r)
                .ok_or_else(|| SellingError::PricingRejected {
                    code: "pricing_line_missing".into(),
                    message: "pricer omitted a line".into(),
                })?;
            let gross = money(pl.unit_price * l.quantity);
            let line_discount = (gross - pl.net_line_total).max(Decimal::ZERO);
            lines.push(NewLine {
                item_id: l.item_id,
                revenue_account_id: l.revenue_account_id,
                description: l.description.clone(),
                quantity: l.quantity,
                unit_price: pl.unit_price,
                line_discount,
            });
        }
        // Buy-X-get-Y: append the free goods as zero-priced lines (they don't change the subtotal).
        for rl in &priced.reward_lines {
            lines.push(NewLine {
                item_id: rl.item_id,
                revenue_account_id: None,
                description: Some("promo reward (free)".into()),
                quantity: rl.quantity,
                unit_price: Decimal::ZERO,
                line_discount: Decimal::ZERO,
            });
        }

        self.create_sales_order(NewSalesOrder {
            order_number: o.order_number,
            quotation_id: None,
            company_id: o.company_id,
            branch_id: o.branch_id,
            customer_id: o.customer_id,
            order_date: o.order_date,
            delivery_date: o.delivery_date,
            currency: o.currency,
            tax_rate: o.tax_rate,
            notes: o.notes,
            lines,
        })
        .await
    }

    /// Confirm a draft order → `to_deliver_and_bill` (awaiting both delivery and billing now that
    /// inventory is live; ADR-003). Reaches `completed` only when fully billed AND fully delivered.
    /// Emits `SalesOrderConfirmed`.
    /// Confirm a draft sales order (draft → to_deliver_and_bill); emits `SalesOrderConfirmed`.
    ///
    /// `company_id` scopes the lookup, so a principal of company A cannot confirm company B's order
    /// by knowing its id — proving *who* the caller is is not enough, the row must be theirs. A
    /// mismatched tenant is indistinguishable from a missing order (`NotDraft`), so this does not
    /// leak whether the id exists.
    pub async fn confirm_sales_order(
        &self,
        order_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), SellingError> {
        // RLS scope (ADR-0008): company on the parameter — scope the guarded update so it runs with
        // `app.company_id` set. The repository holds the statement (and its `company_id=$2`
        // defense-in-depth filter); the scope wrapper stays here, in the service.
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.orders.confirm(&self.db_pool, order_id, company_id),
        ).await?;
        let row = row.ok_or_else(|| SellingError::NotDraft(order_id.to_string()))?;
        self.sink.publish(SellingEvent::SalesOrderConfirmed(SalesOrderConfirmed {
            order_id,
            company_id: row.company_id,
            customer_id: row.customer_id,
            grand_total: row.total,
            currency: row.currency,
        }));
        Ok(())
    }

    /// Accept a quotation (draft/sent → accepted); emits `QuotationAccepted`. Only an accepted
    /// quotation may be converted to a sales order.
    ///
    /// `company_id` scopes the lookup for the same reason as [`Self::confirm_sales_order`]: the
    /// caller's tenant must own the row, not merely be authenticated.
    pub async fn accept_quotation(
        &self,
        quotation_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), SellingError> {
        // RLS scope (ADR-0008): company on the parameter — same shape as `confirm_sales_order`.
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.quotations.accept(&self.db_pool, quotation_id, company_id),
        ).await?;
        let row = row.ok_or_else(|| SellingError::NotDraft(quotation_id.to_string()))?;
        self.sink.publish(SellingEvent::QuotationAccepted(QuotationAccepted {
            quotation_id,
            company_id: row.company_id,
            customer_id: row.customer_id,
        }));
        Ok(())
    }

    /// Convert an accepted quotation into a draft sales order (copies header + lines, links
    /// `quotation_id`, marks the quotation `ordered`). The core Quote→Order step of order-to-cash.
    pub async fn convert_quotation_to_order(
        &self,
        quotation_id: Uuid,
        order_number: String,
    ) -> Result<Uuid, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern: identified by the quotation id alone, with no company
        // argument to scope from up front. These reads therefore ride the REQUEST-dedicated connection
        // (established by `company_auth`), which carries the caller's `app.company_id` — RLS fences the
        // lookup so another company's quotation simply isn't found. `create_sales_order` below binds the
        // quotation's own company onto its transaction.
        let q = self.repos.quotations.find_conversion_source(&self.db_pool, quotation_id).await?
            .ok_or(SellingError::QuotationNotFound(quotation_id))?;
        if q.status != "accepted" {
            return Err(SellingError::QuotationNotAccepted(quotation_id));
        }
        let lines = self.repos.quotation_items.list_for_conversion(&self.db_pool, quotation_id).await?;

        let new_lines: Vec<NewLine> = lines.into_iter().map(|l| NewLine {
            item_id: l.item_id,
            revenue_account_id: None,
            description: l.description,
            quantity: l.quantity,
            unit_price: l.unit_price,
            line_discount: l.line_discount,
        }).collect();

        let order_id = self.create_sales_order(NewSalesOrder {
            order_number,
            quotation_id: Some(quotation_id),
            company_id: q.company_id,
            branch_id: q.branch_id,
            customer_id: q.customer_id,
            order_date: chrono::Utc::now().date_naive(),
            delivery_date: None,
            currency: Some(q.currency),
            tax_rate: q.tax_rate,
            notes: None,
            lines: new_lines,
        }).await?;

        self.repos.quotations.mark_ordered(&self.db_pool, quotation_id).await?;
        Ok(order_id)
    }

    /// Load the exported `SalesOrderRef` (the brief's cross-module DTO) for one order.
    pub async fn sales_order_ref(&self, order_id: Uuid) -> Result<SalesOrderRef, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — see `convert_quotation_to_order`.
        let row = self.repos.orders.find_ref(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        Ok(SalesOrderRef {
            id: order_id,
            customer_id: row.customer_id,
            company_id: row.company_id,
            grand_total: row.total,
            currency: row.currency,
        })
    }

    // ---- Sales Invoice ------------------------------------------------------

    pub async fn create_sales_invoice(&self, inv: NewSalesInvoice) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&inv.lines, inv.tax_rate)?;
        // Every invoice line must carry an income account (the revenue credit target).
        if priced.iter().any(|p| p.revenue_account_id.is_none()) {
            return Err(SellingError::MissingRevenueAccount);
        }
        // If tax is charged, the PPN Output account is mandatory (else the post can't credit it).
        if tax_amount > Decimal::ZERO && inv.tax_output_account_id.is_none() {
            return Err(SellingError::TaxAccountMissing);
        }
        let id = Uuid::new_v4();
        let currency = inv.currency.unwrap_or_else(|| "IDR".into());
        // RLS scope (ADR-0008): bind the invoice's company onto the header+lines transaction.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, inv.company_id).await?;
        let r = self.repos.invoices.insert_draft(&mut tx, &NewSalesInvoiceRow {
            id,
            invoice_number: &inv.invoice_number,
            sales_order_id: inv.sales_order_id,
            company_id: inv.company_id,
            branch_id: inv.branch_id,
            customer_id: inv.customer_id,
            invoice_date: inv.invoice_date,
            due_date: inv.due_date,
            currency: &currency,
            subtotal,
            tax_rate: inv.tax_rate,
            tax_amount,
            total,
            receivable_account_id: inv.receivable_account_id,
            tax_output_account_id: inv.tax_output_account_id,
            notes: inv.notes.as_deref(),
        }).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(inv.invoice_number) } else { e.into() });
        }
        for p in &priced {
            // A directly-raised invoice has no order line to link back to.
            self.repos.invoice_items.insert_line(&mut tx, &NewSalesInvoiceItemRow {
                id: Uuid::new_v4(),
                invoice_id: id,
                company_id: inv.company_id,
                item_id: p.item_id,
                sales_order_item_id: None,
                revenue_account_id: p.revenue_account_id,
                description: p.description.as_deref(),
                quantity: p.quantity,
                unit_price: p.unit_price,
                line_discount: p.line_discount,
                line_amount: p.line_amount,
            }).await?;
        }
        tx.commit().await?;
        self.sink.publish(SellingEvent::SalesInvoiceIssued(SalesInvoiceIssued {
            invoice_id: id,
            sales_order_id: inv.sales_order_id,
            company_id: inv.company_id,
            customer_id: inv.customer_id,
            total,
        }));
        Ok(id)
    }

    /// Raise a sales invoice from a confirmed order: copies the order's lines, links each invoice
    /// line back to its `sales_order_item_id` (so posting advances `billed_qty`), and applies the
    /// supplied GL accounts. `default_revenue_account_id` credits every line (real systems map per
    /// item; a single income account is the SMB default). The core Order→Bill step.
    pub async fn create_invoice_from_order(
        &self,
        order_id: Uuid,
        invoice_number: String,
        invoice_date: chrono::NaiveDate,
        receivable_account_id: Uuid,
        default_revenue_account_id: Uuid,
        tax_output_account_id: Option<Uuid>,
    ) -> Result<Uuid, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern: the order lookup rides the request-dedicated
        // connection; having read the order we bind ITS company onto the invoice transaction below.
        let o = self.repos.orders.find_invoice_source(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        let items = self.repos.order_items.list_for_invoice(&self.db_pool, order_id).await?;
        if items.is_empty() {
            return Err(SellingError::EmptyDocument);
        }

        // Price the order lines the same way (server-side), carrying each SO line id.
        let tax_rate: Decimal = o.tax_rate;
        let mut soi_lines: Vec<(Uuid, PricedLine)> = Vec::new();
        let mut subtotal = Decimal::ZERO;
        for it in items {
            let qty = it.quantity;
            let price = it.unit_price;
            let disc = it.line_discount;
            let line_amount = money(qty * price) - money(disc);
            subtotal += line_amount;
            soi_lines.push((it.id, PricedLine {
                item_id: it.item_id,
                revenue_account_id: Some(default_revenue_account_id),
                description: it.description,
                quantity: qty,
                unit_price: price,
                line_discount: money(disc),
                line_amount,
            }));
        }
        let subtotal = money(subtotal);
        let tax_amount = money(subtotal * tax_rate / Decimal::from(100));
        let total = subtotal + tax_amount;
        if tax_amount > Decimal::ZERO && tax_output_account_id.is_none() {
            return Err(SellingError::TaxAccountMissing);
        }
        let currency: String = o.currency.clone();

        let id = Uuid::new_v4();
        let order_company: Uuid = o.company_id;
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, order_company).await?;
        // An order-raised invoice carries no due date and no notes — the order supplies neither.
        let r = self.repos.invoices.insert_draft(&mut tx, &NewSalesInvoiceRow {
            id,
            invoice_number: &invoice_number,
            sales_order_id: Some(order_id),
            company_id: o.company_id,
            branch_id: o.branch_id,
            customer_id: o.customer_id,
            invoice_date,
            due_date: None,
            currency: &currency,
            subtotal,
            tax_rate,
            tax_amount,
            total,
            receivable_account_id,
            tax_output_account_id,
            notes: None,
        }).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(invoice_number) } else { e.into() });
        }
        for (soi_id, p) in &soi_lines {
            // Link each invoice line back to its order line — this is what lets posting advance
            // that line's `billed_qty`.
            self.repos.invoice_items.insert_line(&mut tx, &NewSalesInvoiceItemRow {
                id: Uuid::new_v4(),
                invoice_id: id,
                company_id: order_company,
                item_id: p.item_id,
                sales_order_item_id: Some(*soi_id),
                revenue_account_id: p.revenue_account_id,
                description: p.description.as_deref(),
                quantity: p.quantity,
                unit_price: p.unit_price,
                line_discount: p.line_discount,
                line_amount: p.line_amount,
            }).await?;
        }
        tx.commit().await?;
        self.sink.publish(SellingEvent::SalesInvoiceIssued(SalesInvoiceIssued {
            invoice_id: id,
            sales_order_id: Some(order_id),
            company_id: o.company_id,
            customer_id: o.customer_id,
            total,
        }));
        Ok(id)
    }

    /// Build the balanced revenue posting envelope for an invoice: Dr A/R (total, with customer
    /// party) · Cr Revenue (per income account, summed) · Cr PPN Output (tax_amount, if any).
    /// Pure + deterministic — the golden oracle asserts these lines directly.
    pub async fn build_revenue_post(&self, invoice_id: Uuid) -> Result<AccountingPostEnvelope, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — see `convert_quotation_to_order`.
        let inv = self.repos.invoices.find_post_source(&self.db_pool, invoice_id).await?
            .ok_or(SellingError::InvoiceNotFound(invoice_id))?;

        let company_id: Uuid = inv.company_id;
        let branch_id: Option<Uuid> = inv.branch_id;
        let customer_id: Uuid = inv.customer_id;
        let invoice_number: String = inv.invoice_number;
        let invoice_date: chrono::NaiveDate = inv.invoice_date;
        let currency: String = inv.currency;
        let tax_amount: Decimal = inv.tax_amount;
        let total: Decimal = inv.total;
        let receivable_account_id: Uuid = inv.receivable_account_id;
        let tax_output_account_id: Option<Uuid> = inv.tax_output_account_id;

        // The GL is kept in the company base currency (IDR) and the envelope carries no
        // exchange_rate (multi-currency is a deferred, separately-designed contract — council
        // 2026-07-03). Refuse to emit a non-IDR post rather than silently booking foreign
        // face-value amounts into an IDR ledger. Backed by a CHECK on selling.sales_invoices.
        if currency != "IDR" {
            return Err(SellingError::UnsupportedCurrency(currency));
        }

        // Credit revenue grouped by income account (BTreeMap → deterministic line order).
        let rows = self.repos.invoice_items.list_revenue_lines(&self.db_pool, invoice_id).await?;
        if rows.is_empty() {
            return Err(SellingError::EmptyDocument);
        }
        let mut revenue: BTreeMap<Uuid, Decimal> = BTreeMap::new();
        for r in &rows {
            *revenue.entry(r.revenue_account_id).or_insert(Decimal::ZERO) += r.line_amount;
        }

        let mut lines: Vec<GlPostLine> = Vec::new();
        // Dr A/R (control) — carries the customer party for subledger aging.
        lines.push(
            GlPostLine::debit(receivable_account_id, total)
                .with_party("customer", customer_id)
                .with_description(format!("A/R {invoice_number}")),
        );
        // Cr Revenue per income account.
        for (acct, amt) in &revenue {
            lines.push(GlPostLine::credit(*acct, *amt).with_description("Revenue"));
        }
        // Cr PPN Output.
        if tax_amount > Decimal::ZERO {
            let tax_acct = tax_output_account_id.ok_or(SellingError::TaxAccountMissing)?;
            lines.push(GlPostLine::credit(tax_acct, tax_amount).with_description("PPN Output"));
        }

        let envelope = AccountingPostEnvelope {
            idempotency_key: invoice_id.to_string(),
            company_id,
            branch_id,
            source_type: "order".into(),
            source_id: invoice_id,
            source_reference: Some(invoice_number),
            posting_date: invoice_date,
            currency,
            posting_type: "original".into(),
            description: Some("Sales invoice revenue".into()),
            lines,
        };
        // Defensive: never emit an unbalanced envelope (would be rejected downstream anyway).
        if !envelope.is_balanced() {
            return Err(SellingError::UnbalancedPost);
        }
        Ok(envelope)
    }

    /// Post an invoice's revenue to the GL through `sink`, then reconcile the invoice from the ack.
    /// Idempotent: a second call on an already-posted invoice returns the recorded ids without
    /// re-emitting. Guarded: only a `draft`/`pending` invoice is posted.
    pub async fn post_sales_invoice(
        &self,
        invoice_id: Uuid,
        sink: &dyn GlPostSink,
    ) -> Result<PostOutcome, SellingError> {
        // Idempotency short-circuit: already posted → return the recorded ids, no re-emit.
        // RLS scope (ADR-0008), ID-only pattern: identified by the invoice id alone. Under HTTP the
        // request-dedicated connection carries the scope. When driven by an EVENT or a job, the caller
        // must wrap this in `with_company_scope(Some(event.company_id))` — otherwise these reads fail
        // closed.
        let existing = self.repos.invoices.find_posting_state(&self.db_pool, invoice_id).await?
            .ok_or(SellingError::InvoiceNotFound(invoice_id))?;
        if existing.posting_state == "posted" {
            if let (Some(j), Some(p)) = (existing.journal_id, existing.accounting_post_id) {
                return Ok(PostOutcome { invoice_id, post_id: p, journal_id: j, idempotent_reuse: true });
            }
        }

        let envelope = self.build_revenue_post(invoice_id).await?;

        // Idempotency note: `envelope.source_id == invoice_id` is the identity accounting dedupes
        // on (its partial unique index on `(company, source_type, source_id, posting_type) WHERE
        // posted`). That index is the authoritative arbiter — two concurrent posts of one invoice
        // yield exactly ONE journal (proven by `gl_posting_seam::concurrent_double_post_*`), because
        // accounting rolls back the loser and returns the winner's ids to both callers. The local
        // guards here (the posted-short-circuit above + the `posting_state <> 'posted'` clause below)
        // are defense-in-depth so selling is self-consistent even if a downstream ever weakened.
        match sink.post(&envelope).await {
            Ok(ack) => {
                self.repos.invoices
                    .reconcile_posted(&self.db_pool, invoice_id, ack.journal_id, ack.post_id)
                    .await?;

                // Advance the source order's billed watermarks (only for a fresh post) and close it
                // out when fully billed. Each invoice line carries its `sales_order_item_id`.
                if !ack.idempotent_reuse {
                    self.advance_billing_watermarks(invoice_id).await?;
                }

                // Read total for the event, then publish SalesInvoicePosted.
                let total: Decimal = self.repos.invoices.fetch_total(&self.db_pool, invoice_id).await?;
                self.sink.publish(SellingEvent::SalesInvoicePosted(SalesInvoicePosted {
                    invoice_id,
                    company_id: envelope.company_id,
                    journal_id: ack.journal_id,
                    post_id: ack.post_id,
                    total,
                }));

                Ok(PostOutcome {
                    invoice_id,
                    post_id: ack.post_id,
                    journal_id: ack.journal_id,
                    idempotent_reuse: ack.idempotent_reuse,
                })
            }
            Err(rej) => {
                // Record the failure so the invoice reflects the rejected post (audit/retry).
                let _ = self.repos.invoices.mark_post_failed(&self.db_pool, invoice_id).await;
                Err(SellingError::GlRejected { code: rej.code, message: rej.message })
            }
        }
    }

    /// For each invoice line linked to a sales-order line, add the invoiced quantity to that SO
    /// line's `billed_qty`; then recompute the order status. No-op for a direct invoice.
    async fn advance_billing_watermarks(&self, invoice_id: Uuid) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — inherits the caller's scope (`post_sales_invoice`).
        // The repo statement is scoped through the INVOICE (its `sales_invoice_items` subquery), not
        // the order — that scoping is deliberate and unchanged.
        self.repos.order_items.advance_billed_from_invoice(&self.db_pool, invoice_id).await?;

        let order_id: Option<Uuid> =
            self.repos.invoices.fetch_sales_order_id(&self.db_pool, invoice_id).await?;
        if let Some(oid) = order_id {
            self.recompute_order_status(oid).await?;
        }
        Ok(())
    }

    /// Recompute an order's status from its two watermarks (ADR-003): `completed` iff every line is
    /// fully billed AND fully delivered; else `to_deliver` (billed, awaiting delivery) / `to_bill`
    /// (delivered, awaiting billing) / `to_deliver_and_bill` (awaiting both). Never touches a
    /// draft/closed/cancelled order.
    async fn recompute_order_status(&self, order_id: Uuid) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — inherits the caller's scope.
        let row = self.repos.order_items.watermark_rollup(&self.db_pool, order_id).await?;
        let next = match (row.billed_all.unwrap_or(false), row.delivered_all.unwrap_or(false)) {
            (true, true) => "completed",
            (true, false) => "to_deliver",
            (false, true) => "to_bill",
            (false, false) => "to_deliver_and_bill",
        };
        // Only advance an in-flight (confirmed) order; leave draft/closed/cancelled alone — the
        // repo statement's `status = ANY(...)` gate is what enforces that.
        self.repos.orders.advance_status(&self.db_pool, order_id, next).await?;
        Ok(())
    }

    // ---- Delivery seam (selling <-> inventory) ------------------------------

    /// Build the cross-module delivery request for a confirmed order (the envelope selling emits;
    /// a fulfillment/composition layer maps it into inventory's `DeliveryRequested`). Emits the
    /// `DeliveryRequested` domain event. Guard: the order must be confirmed (not draft/cancelled).
    pub async fn build_delivery_request(&self, order_id: Uuid) -> Result<DeliveryRequestEnvelope, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern: the reads ride the request-dedicated connection;
        // having read the order we bind ITS company onto the outbox transaction below.
        let hdr = self.repos.orders.find_fulfillment_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        if hdr.status == "draft" {
            return Err(SellingError::NotDraft(order_id.to_string())); // reuse: "not in a confirmable/deliverable state"
        }
        let rows = self.repos.order_items.list_delivery_remainders(&self.db_pool, order_id).await?;
        let lines: Vec<DeliveryRequestLine> = rows.iter().map(|r| DeliveryRequestLine {
            item_id: r.item_id,
            quantity: r.remaining,
        }).collect();
        let env = DeliveryRequestEnvelope {
            order_id,
            company_id: hdr.company_id,
            customer_id: hdr.customer_id,
            currency: hdr.currency,
            lines,
        };
        // Durably stage the cross-module event before the in-proc publish (outbox rollout plan, P1):
        // inventory SUBSCRIBES to DeliveryRequested to move stock + post COGS, so a crash between here and
        // the in-proc publish must not drop it. Staged in its own tx → the relay drains selling.outbox_events;
        // the in-proc publish stays as the fast path.
        let event = SellingEvent::DeliveryRequested(env.clone());
        let record = backbone_outbox::OutboxRecord::new(
            "DeliveryRequested", "SalesOrder", order_id.to_string(),
            serde_json::to_value(&event).map_err(|e| SellingError::Outbox(e.to_string()))?,
            chrono::Utc::now(),
        );
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, env.company_id).await?;
        backbone_outbox::outbox::stage(&mut *tx, "selling", &record)
            .await.map_err(|e| SellingError::Outbox(format!("stage: {e}")))?;
        tx.commit().await?;
        self.sink.publish(event);
        Ok(env)
    }

    /// Record a delivery against an order (the inbound handler for inventory's `StockDelivered`):
    /// advance `delivered_qty` per item and recompute the order status. Matches by `item_id`.
    pub async fn mark_delivered(&self, order_id: Uuid, deliveries: &[(Uuid, Decimal)]) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern: no company is available here, and this is the inbound
        // handler for inventory's `StockDelivered` — the CALLER must wrap this in
        // `with_company_scope(Some(event.company_id))`, otherwise these writes fail closed.
        for (item_id, qty) in deliveries {
            self.repos.order_items
                .add_delivered_qty(&self.db_pool, order_id, *item_id, *qty)
                .await?;
        }
        self.recompute_order_status(order_id).await?;
        Ok(())
    }

    /// Build the invoice request for a confirmed order (the order-to-cash mirror of
    /// `build_delivery_request`): asks billing to invoice only the **un-invoiced remainder**
    /// (`quantity − billed_qty`) per line, carrying the unit price. A composition layer maps the
    /// emitted `OrderInvoiced` envelope into billing's `NewSalesInvoice` (adding the A/R + revenue
    /// accounts) and posts the real revenue journal — so selling no longer owns invoicing or posts
    /// revenue itself (retiring `create_invoice_from_order` in the composed flow).
    pub async fn build_invoice_request(&self, order_id: Uuid) -> Result<InvoiceRequestEnvelope, SellingError> {
        // RLS scope (ADR-0008), ID-only pattern — see `build_delivery_request`. Read-only.
        let hdr = self.repos.orders.find_fulfillment_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        if hdr.status == "draft" {
            return Err(SellingError::NotDraft(order_id.to_string()));
        }
        let rows = self.repos.order_items.list_billing_remainders(&self.db_pool, order_id).await?;
        let lines: Vec<InvoiceRequestLine> = rows.iter().map(|r| InvoiceRequestLine {
            item_id: r.item_id,
            quantity: r.remaining,
            unit_price: r.unit_price,
        }).collect();
        let env = InvoiceRequestEnvelope {
            order_id,
            company_id: hdr.company_id,
            customer_id: hdr.customer_id,
            currency: hdr.currency,
            lines,
        };
        self.sink.publish(SellingEvent::OrderInvoiced(env.clone()));
        Ok(env)
    }

    /// Record that an order was invoiced (the inbound handler for billing's `SalesInvoicePosted`):
    /// advance `billed_qty` per item and recompute the order status. The order-to-cash mirror of
    /// buying's `mark_billed` (council 2026-07-05): **bounded** — it routes through a capacity-checked,
    /// `FOR UPDATE`-serialized allocation capped at each line's `quantity`, and **rejects** an over-bill
    /// (`OverBilled`). Without this, a racy/repeat `build_invoice_request` (billed_qty advances only at
    /// post time) or a directly-raised invoice could push `billed_qty` past `quantity` — booking revenue
    /// beyond the order while `recompute_order_status` (`billed_qty ≥ quantity`) silently masks it as
    /// `completed`. Serializing the *writer* (not just the upstream remainder) is what closes the race.
    /// Aggregate-by-item, fill in line order — correct even for duplicate-item orders.
    pub async fn mark_invoiced(&self, order_id: Uuid, billed: &[(Uuid, Decimal)]) -> Result<(), SellingError> {
        // RLS scope (ADR-0008), ID-only pattern: this method has NO company available — it is the
        // inbound handler for billing's `SalesInvoicePosted`, so the allocation tx binds the AMBIENT
        // task-local company. The CALLER must wrap this in `with_company_scope(Some(event.company_id))`
        // — the event carries the company — otherwise the `FOR UPDATE` reads inside `allocate_billed`
        // fail closed and every allocation would read zero capacity.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_current_company(&mut tx).await?;
        for (item_id, qty) in billed {
            self.allocate_billed(&mut tx, order_id, *item_id, *qty).await?;
        }
        tx.commit().await?;
        self.recompute_order_status(order_id).await?;
        Ok(())
    }

    /// Fill `billed_qty` up to `quantity` across an item's order lines (`FOR UPDATE`, fill-in-order);
    /// reject when the requested qty exceeds total remaining capacity (`quantity − billed_qty`).
    ///
    /// The lock-read and the writes MUST share the caller's `tx` — that is what serializes concurrent
    /// billers; splitting them across connections would re-open the over-bill race.
    async fn allocate_billed(&self, tx: &mut sqlx::PgConnection, order_id: Uuid, item_id: Uuid, mut qty: Decimal) -> Result<(), SellingError> {
        let lines = self.repos.order_items.lock_billing_capacity(&mut *tx, order_id, item_id).await?;
        let total_cap: Decimal = lines.iter().map(|r| r.capacity).sum();
        if qty > total_cap {
            return Err(SellingError::OverBilled);
        }
        for line in &lines {
            if qty <= Decimal::ZERO { break; }
            let cap = line.capacity;
            if cap <= Decimal::ZERO { continue; }
            let take = if qty < cap { qty } else { cap };
            self.repos.order_items.add_billed_qty(&mut *tx, line.id, take).await?;
            qty -= take;
        }
        Ok(())
    }
}
