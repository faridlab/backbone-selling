//! Sales invoice creation: direct + from-order (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `create_sales_invoice` prices lines server-side, requires a revenue account per line, and
//! requires the PPN Output account iff the computed tax > 0. `create_invoice_from_order` copies a
//! confirmed order's lines, links each invoice line back to its `sales_order_item_id` (so posting
//! advances `billed_qty`), and applies the supplied GL accounts. Both write header + lines as ONE
//! transaction.
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `SalesInvoiceRepository` / `SalesInvoiceItemRepository` (and `SalesOrderRepository` /
//! `SalesOrderItemRepository` for the from-order read).

use backbone_orm::company_scope;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::infrastructure::persistence::{NewSalesInvoiceItemRow, NewSalesInvoiceRow};

use super::selling_events::{SalesInvoiceIssued, SellingEvent};
use super::selling_write_service::{
    is_dup, money, price_document, NewSalesInvoice, PricedLine, SellingError, SellingWriteService,
};

impl SellingWriteService {
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
}
