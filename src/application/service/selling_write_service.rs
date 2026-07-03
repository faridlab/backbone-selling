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

use rust_decimal::{Decimal, RoundingStrategy};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

use super::selling_events::{
    QuotationAccepted, SalesInvoiceIssued, SalesInvoicePosted, SalesOrderConfirmed, SalesOrderRef,
    SellingEvent, SellingEventSink, LoggingSink,
};
use super::selling_gl::{AccountingPostEnvelope, GlPostLine, GlPostSink};

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
    GlRejected { code: String, message: String },
    Db(sqlx::Error),
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
            // Surface the GL's own stable code so callers see one contract vocabulary.
            SellingError::GlRejected { code, .. } => code.clone(),
            SellingError::Db(_) => "internal_error".into(),
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            SellingError::InvoiceNotFound(_)
            | SellingError::QuotationNotFound(_)
            | SellingError::OrderNotFound(_) => 404,
            SellingError::Db(_) => 500,
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

#[derive(Clone)]
pub struct SellingWriteService {
    db_pool: PgPool,
    sink: Arc<dyn SellingEventSink>,
}

impl SellingWriteService {
    pub fn new(db_pool: PgPool) -> Self {
        Self { db_pool, sink: Arc::new(LoggingSink) }
    }

    /// Construct with a custom domain-event sink (a bus adapter, or a test recorder / consumer rule).
    pub fn with_sink(db_pool: PgPool, sink: Arc<dyn SellingEventSink>) -> Self {
        Self { db_pool, sink }
    }

    // ---- Quotation ----------------------------------------------------------

    pub async fn create_quotation(&self, q: NewQuotation) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&q.lines, q.tax_rate)?;
        let id = Uuid::new_v4();
        let currency = q.currency.unwrap_or_else(|| "IDR".into());
        let mut tx = self.db_pool.begin().await?;
        let r = sqlx::query(
            r#"INSERT INTO selling.quotations
                (id, quotation_number, company_id, branch_id, customer_id, status, quotation_date,
                 valid_until, currency, subtotal, tax_rate, tax_amount, total, notes)
               VALUES ($1,$2,$3,$4,$5,'draft'::quotation_status,$6,$7,$8,$9,$10,$11,$12,$13)"#,
        )
        .bind(id).bind(&q.quotation_number).bind(q.company_id).bind(q.branch_id).bind(q.customer_id)
        .bind(q.quotation_date).bind(q.valid_until).bind(&currency)
        .bind(subtotal).bind(q.tax_rate).bind(tax_amount).bind(total).bind(&q.notes)
        .execute(&mut *tx).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(q.quotation_number) } else { e.into() });
        }
        for p in &priced {
            sqlx::query(
                r#"INSERT INTO selling.quotation_items
                    (id, quotation_id, item_id, description, quantity, unit_price, line_discount, line_amount)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"#,
            )
            .bind(Uuid::new_v4()).bind(id).bind(p.item_id).bind(&p.description)
            .bind(p.quantity).bind(p.unit_price).bind(p.line_discount).bind(p.line_amount)
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    // ---- Sales Order --------------------------------------------------------

    pub async fn create_sales_order(&self, o: NewSalesOrder) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&o.lines, o.tax_rate)?;
        let id = Uuid::new_v4();
        let currency = o.currency.unwrap_or_else(|| "IDR".into());
        let mut tx = self.db_pool.begin().await?;
        let r = sqlx::query(
            r#"INSERT INTO selling.sales_orders
                (id, order_number, quotation_id, company_id, branch_id, customer_id, status,
                 order_date, delivery_date, currency, subtotal, tax_rate, tax_amount, total, notes)
               VALUES ($1,$2,$3,$4,$5,$6,'draft'::sales_order_status,$7,$8,$9,$10,$11,$12,$13,$14)"#,
        )
        .bind(id).bind(&o.order_number).bind(o.quotation_id).bind(o.company_id).bind(o.branch_id)
        .bind(o.customer_id).bind(o.order_date).bind(o.delivery_date).bind(&currency)
        .bind(subtotal).bind(o.tax_rate).bind(tax_amount).bind(total).bind(&o.notes)
        .execute(&mut *tx).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(o.order_number) } else { e.into() });
        }
        for p in &priced {
            sqlx::query(
                r#"INSERT INTO selling.sales_order_items
                    (id, order_id, item_id, description, quantity, unit_price, line_discount, line_amount)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"#,
            )
            .bind(Uuid::new_v4()).bind(id).bind(p.item_id).bind(&p.description)
            .bind(p.quantity).bind(p.unit_price).bind(p.line_discount).bind(p.line_amount)
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Confirm a draft order → `to_bill` (no delivery tracking until inventory lands; a fully
    /// delivered-and-billed order later reaches `completed`). Emits `SalesOrderConfirmed`.
    pub async fn confirm_sales_order(&self, order_id: Uuid) -> Result<(), SellingError> {
        let row = sqlx::query(
            r#"UPDATE selling.sales_orders SET status='to_bill'::sales_order_status
               WHERE id=$1 AND status='draft'::sales_order_status AND (metadata->>'deleted_at') IS NULL
               RETURNING company_id, customer_id, total, currency"#,
        )
        .bind(order_id).fetch_optional(&self.db_pool).await?;
        let row = row.ok_or_else(|| SellingError::NotDraft(order_id.to_string()))?;
        self.sink.publish(SellingEvent::SalesOrderConfirmed(SalesOrderConfirmed {
            order_id,
            company_id: row.get("company_id"),
            customer_id: row.get("customer_id"),
            grand_total: row.get("total"),
            currency: row.get("currency"),
        }));
        Ok(())
    }

    /// Accept a quotation (draft/sent → accepted); emits `QuotationAccepted`. Only an accepted
    /// quotation may be converted to a sales order.
    pub async fn accept_quotation(&self, quotation_id: Uuid) -> Result<(), SellingError> {
        let row = sqlx::query(
            r#"UPDATE selling.quotations SET status='accepted'::quotation_status
               WHERE id=$1 AND status = ANY(ARRAY['draft','sent']::quotation_status[])
                 AND (metadata->>'deleted_at') IS NULL
               RETURNING company_id, customer_id"#,
        )
        .bind(quotation_id).fetch_optional(&self.db_pool).await?;
        let row = row.ok_or_else(|| SellingError::NotDraft(quotation_id.to_string()))?;
        self.sink.publish(SellingEvent::QuotationAccepted(QuotationAccepted {
            quotation_id,
            company_id: row.get("company_id"),
            customer_id: row.get("customer_id"),
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
        let q = sqlx::query(
            r#"SELECT company_id, branch_id, customer_id, currency, tax_rate, status::text AS st
               FROM selling.quotations WHERE id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(quotation_id).fetch_optional(&self.db_pool).await?
        .ok_or(SellingError::QuotationNotFound(quotation_id))?;
        if q.get::<String, _>("st") != "accepted" {
            return Err(SellingError::QuotationNotAccepted(quotation_id));
        }
        let lines = sqlx::query(
            r#"SELECT item_id, description, quantity, unit_price, line_discount
               FROM selling.quotation_items WHERE quotation_id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(quotation_id).fetch_all(&self.db_pool).await?;

        let new_lines: Vec<NewLine> = lines.iter().map(|l| NewLine {
            item_id: l.get("item_id"),
            revenue_account_id: None,
            description: l.get("description"),
            quantity: l.get("quantity"),
            unit_price: l.get("unit_price"),
            line_discount: l.get("line_discount"),
        }).collect();

        let order_id = self.create_sales_order(NewSalesOrder {
            order_number,
            quotation_id: Some(quotation_id),
            company_id: q.get("company_id"),
            branch_id: q.get("branch_id"),
            customer_id: q.get("customer_id"),
            order_date: chrono::Utc::now().date_naive(),
            delivery_date: None,
            currency: Some(q.get("currency")),
            tax_rate: q.get("tax_rate"),
            notes: None,
            lines: new_lines,
        }).await?;

        sqlx::query(
            r#"UPDATE selling.quotations SET status='ordered'::quotation_status WHERE id=$1"#,
        )
        .bind(quotation_id).execute(&self.db_pool).await?;
        Ok(order_id)
    }

    /// Load the exported `SalesOrderRef` (the brief's cross-module DTO) for one order.
    pub async fn sales_order_ref(&self, order_id: Uuid) -> Result<SalesOrderRef, SellingError> {
        let row = sqlx::query(
            r#"SELECT customer_id, company_id, total, currency FROM selling.sales_orders
               WHERE id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(order_id).fetch_optional(&self.db_pool).await?
        .ok_or(SellingError::OrderNotFound(order_id))?;
        Ok(SalesOrderRef {
            id: order_id,
            customer_id: row.get("customer_id"),
            company_id: row.get("company_id"),
            grand_total: row.get("total"),
            currency: row.get("currency"),
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
        let mut tx = self.db_pool.begin().await?;
        let r = sqlx::query(
            r#"INSERT INTO selling.sales_invoices
                (id, invoice_number, sales_order_id, company_id, branch_id, customer_id, status,
                 invoice_date, due_date, currency, subtotal, tax_rate, tax_amount, total,
                 outstanding_amount, receivable_account_id, tax_output_account_id, posting_state, notes)
               VALUES ($1,$2,$3,$4,$5,$6,'draft'::sales_invoice_status,$7,$8,$9,$10,$11,$12,$13,
                       0,$14,$15,'pending'::gl_posting_state,$16)"#,
        )
        .bind(id).bind(&inv.invoice_number).bind(inv.sales_order_id).bind(inv.company_id)
        .bind(inv.branch_id).bind(inv.customer_id).bind(inv.invoice_date).bind(inv.due_date)
        .bind(&currency).bind(subtotal).bind(inv.tax_rate).bind(tax_amount).bind(total)
        .bind(inv.receivable_account_id).bind(inv.tax_output_account_id).bind(&inv.notes)
        .execute(&mut *tx).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(inv.invoice_number) } else { e.into() });
        }
        for p in &priced {
            sqlx::query(
                r#"INSERT INTO selling.sales_invoice_items
                    (id, invoice_id, item_id, revenue_account_id, description, quantity, unit_price,
                     line_discount, line_amount)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)"#,
            )
            .bind(Uuid::new_v4()).bind(id).bind(p.item_id).bind(p.revenue_account_id)
            .bind(&p.description).bind(p.quantity).bind(p.unit_price).bind(p.line_discount).bind(p.line_amount)
            .execute(&mut *tx).await?;
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
        let o = sqlx::query(
            r#"SELECT company_id, branch_id, customer_id, currency, tax_rate
               FROM selling.sales_orders WHERE id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(order_id).fetch_optional(&self.db_pool).await?
        .ok_or(SellingError::OrderNotFound(order_id))?;
        let items = sqlx::query(
            r#"SELECT id, item_id, description, quantity, unit_price, line_discount
               FROM selling.sales_order_items WHERE order_id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(order_id).fetch_all(&self.db_pool).await?;
        if items.is_empty() {
            return Err(SellingError::EmptyDocument);
        }

        // Price the order lines the same way (server-side), carrying each SO line id.
        let tax_rate: Decimal = o.get("tax_rate");
        let mut soi_lines: Vec<(Uuid, PricedLine)> = Vec::new();
        let mut subtotal = Decimal::ZERO;
        for it in &items {
            let qty: Decimal = it.get("quantity");
            let price: Decimal = it.get("unit_price");
            let disc: Decimal = it.get("line_discount");
            let line_amount = money(qty * price) - money(disc);
            subtotal += line_amount;
            soi_lines.push((it.get("id"), PricedLine {
                item_id: it.get("item_id"),
                revenue_account_id: Some(default_revenue_account_id),
                description: it.get("description"),
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
        let currency: String = o.get("currency");

        let id = Uuid::new_v4();
        let mut tx = self.db_pool.begin().await?;
        let r = sqlx::query(
            r#"INSERT INTO selling.sales_invoices
                (id, invoice_number, sales_order_id, company_id, branch_id, customer_id, status,
                 invoice_date, due_date, currency, subtotal, tax_rate, tax_amount, total,
                 outstanding_amount, receivable_account_id, tax_output_account_id, posting_state, notes)
               VALUES ($1,$2,$3,$4,$5,$6,'draft'::sales_invoice_status,$7,NULL,$8,$9,$10,$11,$12,
                       0,$13,$14,'pending'::gl_posting_state,NULL)"#,
        )
        .bind(id).bind(&invoice_number).bind(order_id).bind(o.get::<Uuid, _>("company_id"))
        .bind(o.get::<Option<Uuid>, _>("branch_id")).bind(o.get::<Uuid, _>("customer_id"))
        .bind(invoice_date).bind(&currency).bind(subtotal).bind(tax_rate).bind(tax_amount).bind(total)
        .bind(receivable_account_id).bind(tax_output_account_id)
        .execute(&mut *tx).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::DuplicateNumber(invoice_number) } else { e.into() });
        }
        for (soi_id, p) in &soi_lines {
            sqlx::query(
                r#"INSERT INTO selling.sales_invoice_items
                    (id, invoice_id, item_id, sales_order_item_id, revenue_account_id, description,
                     quantity, unit_price, line_discount, line_amount)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)"#,
            )
            .bind(Uuid::new_v4()).bind(id).bind(p.item_id).bind(soi_id).bind(p.revenue_account_id)
            .bind(&p.description).bind(p.quantity).bind(p.unit_price).bind(p.line_discount).bind(p.line_amount)
            .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        self.sink.publish(SellingEvent::SalesInvoiceIssued(SalesInvoiceIssued {
            invoice_id: id,
            sales_order_id: Some(order_id),
            company_id: o.get("company_id"),
            customer_id: o.get("customer_id"),
            total,
        }));
        Ok(id)
    }

    /// Build the balanced revenue posting envelope for an invoice: Dr A/R (total, with customer
    /// party) · Cr Revenue (per income account, summed) · Cr PPN Output (tax_amount, if any).
    /// Pure + deterministic — the golden oracle asserts these lines directly.
    pub async fn build_revenue_post(&self, invoice_id: Uuid) -> Result<AccountingPostEnvelope, SellingError> {
        let inv = sqlx::query(
            r#"SELECT invoice_number, company_id, branch_id, customer_id, invoice_date, currency,
                      subtotal, tax_amount, total, receivable_account_id, tax_output_account_id
               FROM selling.sales_invoices
               WHERE id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(invoice_id).fetch_optional(&self.db_pool).await?
        .ok_or(SellingError::InvoiceNotFound(invoice_id))?;

        let company_id: Uuid = inv.get("company_id");
        let branch_id: Option<Uuid> = inv.get("branch_id");
        let customer_id: Uuid = inv.get("customer_id");
        let invoice_number: String = inv.get("invoice_number");
        let invoice_date: chrono::NaiveDate = inv.get("invoice_date");
        let currency: String = inv.get("currency");
        let tax_amount: Decimal = inv.get("tax_amount");
        let total: Decimal = inv.get("total");
        let receivable_account_id: Uuid = inv.get("receivable_account_id");
        let tax_output_account_id: Option<Uuid> = inv.get("tax_output_account_id");

        // The GL is kept in the company base currency (IDR) and the envelope carries no
        // exchange_rate (multi-currency is a deferred, separately-designed contract — council
        // 2026-07-03). Refuse to emit a non-IDR post rather than silently booking foreign
        // face-value amounts into an IDR ledger. Backed by a CHECK on selling.sales_invoices.
        if currency != "IDR" {
            return Err(SellingError::UnsupportedCurrency(currency));
        }

        // Credit revenue grouped by income account (BTreeMap → deterministic line order).
        let rows = sqlx::query(
            r#"SELECT revenue_account_id, line_amount FROM selling.sales_invoice_items
               WHERE invoice_id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(invoice_id).fetch_all(&self.db_pool).await?;
        if rows.is_empty() {
            return Err(SellingError::EmptyDocument);
        }
        let mut revenue: BTreeMap<Uuid, Decimal> = BTreeMap::new();
        for r in &rows {
            let acct: Uuid = r.get("revenue_account_id");
            let amt: Decimal = r.get("line_amount");
            *revenue.entry(acct).or_insert(Decimal::ZERO) += amt;
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
        let existing = sqlx::query(
            r#"SELECT posting_state::text AS ps, journal_id, accounting_post_id
               FROM selling.sales_invoices WHERE id=$1 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(invoice_id).fetch_optional(&self.db_pool).await?
        .ok_or(SellingError::InvoiceNotFound(invoice_id))?;
        let state: String = existing.get("ps");
        if state == "posted" {
            let journal_id: Option<Uuid> = existing.get("journal_id");
            let post_id: Option<Uuid> = existing.get("accounting_post_id");
            if let (Some(j), Some(p)) = (journal_id, post_id) {
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
                sqlx::query(
                    r#"UPDATE selling.sales_invoices
                       SET posting_state='posted'::gl_posting_state,
                           status='submitted'::sales_invoice_status,
                           journal_id=$2, accounting_post_id=$3, posted_at=now(),
                           outstanding_amount=total
                       WHERE id=$1 AND posting_state <> 'posted'::gl_posting_state"#,
                )
                .bind(invoice_id).bind(ack.journal_id).bind(ack.post_id)
                .execute(&self.db_pool).await?;

                // Advance the source order's billed watermarks (only for a fresh post) and close it
                // out when fully billed. Each invoice line carries its `sales_order_item_id`.
                if !ack.idempotent_reuse {
                    self.advance_billing_watermarks(invoice_id).await?;
                }

                // Read total for the event, then publish SalesInvoicePosted.
                let total: Decimal = sqlx::query_scalar(
                    "SELECT total FROM selling.sales_invoices WHERE id=$1",
                )
                .bind(invoice_id).fetch_one(&self.db_pool).await?;
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
                let _ = sqlx::query(
                    r#"UPDATE selling.sales_invoices SET posting_state='failed'::gl_posting_state WHERE id=$1"#,
                )
                .bind(invoice_id).execute(&self.db_pool).await;
                Err(SellingError::GlRejected { code: rej.code, message: rej.message })
            }
        }
    }

    /// For each invoice line linked to a sales-order line, add the invoiced quantity to that SO
    /// line's `billed_qty`; then, if every line of the order is fully billed, advance the order to
    /// `completed`. No-op for a direct invoice (no `sales_order_item_id`).
    async fn advance_billing_watermarks(&self, invoice_id: Uuid) -> Result<(), SellingError> {
        // Bump billed_qty on the linked SO lines.
        sqlx::query(
            r#"UPDATE selling.sales_order_items soi
               SET billed_qty = soi.billed_qty + ii.qty
               FROM (SELECT sales_order_item_id AS soi_id, SUM(quantity) AS qty
                     FROM selling.sales_invoice_items
                     WHERE invoice_id=$1 AND sales_order_item_id IS NOT NULL
                       AND (metadata->>'deleted_at') IS NULL
                     GROUP BY sales_order_item_id) ii
               WHERE soi.id = ii.soi_id"#,
        )
        .bind(invoice_id).execute(&self.db_pool).await?;

        // Find the source order (if any) and close it out when fully billed.
        let order_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT sales_order_id FROM selling.sales_invoices WHERE id=$1",
        )
        .bind(invoice_id).fetch_one(&self.db_pool).await?;
        if let Some(oid) = order_id {
            let fully_billed: bool = sqlx::query_scalar(
                r#"SELECT bool_and(billed_qty >= quantity) FROM selling.sales_order_items
                   WHERE order_id=$1 AND (metadata->>'deleted_at') IS NULL"#,
            )
            .bind(oid).fetch_one(&self.db_pool).await?;
            if fully_billed {
                // to_bill → completed (delivery not tracked yet; inventory will gate to_deliver*).
                sqlx::query(
                    r#"UPDATE selling.sales_orders SET status='completed'::sales_order_status
                       WHERE id=$1 AND status='to_bill'::sales_order_status"#,
                )
                .bind(oid).execute(&self.db_pool).await?;
            }
        }
        Ok(())
    }
}
