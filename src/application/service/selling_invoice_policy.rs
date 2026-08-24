//! The invoicing-policy engine (hand-authored, user-owned) — PURE COMPUTE, single source.
//!
//! A line's `invoice_policy` decides WHEN its quantity becomes invoiceable:
//!
//! - `order`     — invoiceable on confirmation; the basis is the ordered `quantity`.
//! - `delivery`  — invoiceable on delivery; the basis is `delivered_qty`.
//!
//! The canonical basis is ONE expression, mirrored in the four SQL sites and here:
//!
//! ```text
//! policy_base(line) = (invoice_policy == 'delivery' AND NOT is_downpayment) ? delivered_qty
//!                                                                     : quantity
//! qty_to_invoice(line) = policy_base − billed_qty        // raw; may be negative (upselling)
//! ```
//!
//! The mirrored SQL sites (in `sales_order_item_repository.rs`):
//! `list_billing_remainders` (the invoice REQUEST quantities), `lock_billing_capacity` (the billed
//! WATERMARK bound — so a delivery-policy line can never be billed past its delivered quantity),
//! and `watermark_rollup` (the STATUS recompute — a delivery-policy line is "fully billed" at
//! `billed_qty >= delivered_qty`, which is what un-strands partially delivered orders from
//! `to_deliver_and_bill`). Keeping the expression identical in all four places is the invariant
//! this file documents; drift here strands orders or over/under-requests invoices.
//!
//! `is_downpayment` lines always stay on the quantity basis (billing's downpayment advances
//! precede delivery) but are EXCLUDED from the order-level aggregate and from the status rollup.
//!
//! `qty_to_invoice` / `invoice_status` are COMPUTED at read time and exposed on the invoice-status
//! read DTOs — they are never persisted and no write route accepts them (structurally: they are
//! not schema fields).
//!
//! Status vocabulary (Odoo's, adapted):
//! - line:   `no` | `to invoice` | `invoiced` | `upselling`
//!   (`upselling` = billed exceeds ordered on the ordered basis — more invoiced than was ordered.)
//! - order:  the aggregate over the non-downpayment lines, actionable-first: any `to invoice`
//!   wins over any `upselling` (deliberate delta from Odoo's loop, where a late upselling line can
//!   overwrite `to invoice`; the aggregate's job is "what do I do next").
//!
//! Per the module's 4-layer rule this file holds no SQL — the reads live on
//! `SalesOrderItemRepository` / `QuotationItemRepository`.

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::infrastructure::persistence::InvoicePolicyOrderLineRow;

use super::selling_write_service::{SellingError, SellingWriteService};

/// The Rust mirror of the canonical SQL basis: `delivered_qty` for a delivery-policy line that is
/// not a downpayment, `quantity` otherwise.
pub(crate) fn policy_base(invoice_policy: &str, is_downpayment: bool, delivered_qty: Decimal, quantity: Decimal) -> Decimal {
    if invoice_policy == "delivery" && !is_downpayment {
        delivered_qty
    } else {
        quantity
    }
}

/// `qty_to_invoice` = `policy_base − billed_qty`, RAW: negative on upselling (billed beyond the
/// ordered quantity) or returns. The invoice REQUEST path filters to the positive remainder; the
/// read DTO exposes the raw value.
pub(crate) fn qty_to_invoice(policy_base: Decimal, billed_qty: Decimal) -> Decimal {
    policy_base - billed_qty
}

/// One line's invoice status (only meaningful on a confirmed order; a draft/cancelled/closed
/// order's lines are all `no`).
pub(crate) fn line_invoice_status(
    order_status: &str,
    invoice_policy: &str,
    is_downpayment: bool,
    quantity: Decimal,
    delivered_qty: Decimal,
    billed_qty: Decimal,
) -> &'static str {
    if matches!(order_status, "draft" | "cancelled" | "closed") {
        return "no";
    }
    let base = policy_base(invoice_policy, is_downpayment, delivered_qty, quantity);
    let to_invoice = qty_to_invoice(base, billed_qty);
    if to_invoice > Decimal::ZERO {
        "to invoice"
    } else if base == quantity && billed_qty > quantity {
        // Billed exceeds ordered on the ordered basis (downpayment or order-policy line).
        "upselling"
    } else if billed_qty > Decimal::ZERO {
        // Fully billed for everything billable right now. A delivery-policy line partially
        // delivered + fully billed-to-delivered lands here (delivered 6 of 10, billed 6): billing
        // is done until more delivery lands, at which point `to_invoice > 0` reopens it.
        "invoiced"
    } else {
        // Nothing billed and nothing billable: a delivery-policy line awaiting its first
        // delivery. `no`, not `invoiced` — "invoiced" claims work that was never done.
        "no"
    }
}

/// The order-level aggregate over the NON-downpayment lines, actionable-first: `to invoice`
/// outranks `upselling` (deliberate delta from Odoo — the aggregate answers "what do I do next").
/// An order with no non-downpayment lines aggregates to `no`.
pub(crate) fn aggregate_invoice_status(line_statuses: &[&'static str]) -> &'static str {
    let mut any_to_invoice = false;
    let mut any_upselling = false;
    let mut all_invoiced = true;
    for s in line_statuses {
        match *s {
            "to invoice" => { any_to_invoice = true; all_invoiced = false; }
            "upselling" => { any_upselling = true; all_invoiced = false; }
            "invoiced" => {}
            _ => { all_invoiced = false; } // "no"
        }
    }
    if any_to_invoice {
        "to invoice"
    } else if any_upselling {
        "upselling"
    } else if all_invoiced && !line_statuses.is_empty() {
        "invoiced"
    } else {
        "no"
    }
}

impl SellingWriteService {
    /// The order invoice-status read model: per-line `qty_to_invoice` + `invoice_status` computes
    /// and the downpayment-excluding aggregate. Pure compute over the persisted watermarks —
    /// nothing here is stored or writable. ID-only read (rides the caller's request-scoped
    /// connection; RLS fences it).
    pub async fn order_invoice_view(
        &self,
        order_id: Uuid,
    ) -> Result<SalesOrderInvoiceStatusDto, SellingError> {
        let hdr = self.repos.orders.find_invoice_status_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        let rows = self.repos.order_items.list_invoice_policy_rows(&self.db_pool, order_id).await?;

        let lines: Vec<SalesOrderItemInvoiceDto> = rows.iter().map(|r| {
            let st = line_invoice_status(&hdr.status, &r.invoice_policy, r.is_downpayment, r.quantity, r.delivered_qty, r.billed_qty);
            SalesOrderItemInvoiceDto {
                id: r.id,
                item_id: r.item_id,
                invoice_policy: r.invoice_policy.clone(),
                is_downpayment: r.is_downpayment,
                quantity: r.quantity,
                delivered_qty: r.delivered_qty,
                billed_qty: r.billed_qty,
                qty_to_invoice: qty_to_invoice(
                    policy_base(&r.invoice_policy, r.is_downpayment, r.delivered_qty, r.quantity),
                    r.billed_qty,
                ),
                invoice_status: st.to_string(),
            }
        }).collect();

        let aggregate = aggregate_invoice_status(
            &rows.iter()
                .filter(|r| !r.is_downpayment)
                .map(|r| line_invoice_status(&hdr.status, &r.invoice_policy, r.is_downpayment, r.quantity, r.delivered_qty, r.billed_qty))
                .collect::<Vec<_>>(),
        );

        Ok(SalesOrderInvoiceStatusDto {
            order_id,
            order_number: hdr.order_number,
            status: hdr.status,
            invoice_status: aggregate.to_string(),
            lines,
        })
    }

    /// The quotation invoice-status read model. Quotation lines carry no watermarks: per line
    /// `qty_to_invoice = quantity − 0` and the status is always `no` — nothing is invoiceable
    /// before an order exists. The endpoint's value is surfacing the policy + downpayment flags
    /// conversion will carry onto the order lines.
    pub async fn quotation_invoice_view(
        &self,
        quotation_id: Uuid,
    ) -> Result<QuotationInvoiceStatusDto, SellingError> {
        let hdr = self.repos.quotations.find_invoice_status_header(&self.db_pool, quotation_id).await?
            .ok_or(SellingError::QuotationNotFound(quotation_id))?;
        let rows = self.repos.quotation_items.list_policy_rows(&self.db_pool, quotation_id).await?;

        let lines: Vec<QuotationItemInvoiceDto> = rows.iter().map(|r| QuotationItemInvoiceDto {
            id: r.id,
            item_id: r.item_id,
            invoice_policy: r.invoice_policy.clone(),
            is_downpayment: r.is_downpayment,
            quantity: r.quantity,
            qty_to_invoice: r.quantity, // no watermarks pre-order
            invoice_status: "no".into(),
        }).collect();

        Ok(QuotationInvoiceStatusDto {
            quotation_id,
            quotation_number: hdr.quotation_number,
            status: hdr.status,
            invoice_status: "no".into(),
            lines,
        })
    }
}

// ── the invoicing-policy read DTOs ─────────────────────────────────────────────
//
// These live HERE (the user-owned policy file), not in the generated `presentation/dto/*.rs`
// files: a schema regen rewrites those files wholesale and eats hand-appended blocks — including
// ones wrapped in CUSTOM markers at end-of-file (observed live). The policy engine owns these
// shapes outright; nothing generator-owned references them.

/// One order line's invoicing-policy read: the COMPUTED `qty_to_invoice` / `invoice_status` pair
/// (pure compute at read time — never persisted, no write route can set them; see ADR-007).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SalesOrderItemInvoiceDto {
    pub id: uuid::Uuid,
    pub item_id: uuid::Uuid,
    /// "order" | "delivery"
    pub invoice_policy: String,
    pub is_downpayment: bool,
    pub quantity: rust_decimal::Decimal,
    pub delivered_qty: rust_decimal::Decimal,
    pub billed_qty: rust_decimal::Decimal,
    /// `policy_base − billed_qty` — RAW (may be negative on upselling/returns); the invoice
    /// request path filters to the positive remainder.
    pub qty_to_invoice: rust_decimal::Decimal,
    /// "no" | "to invoice" | "invoiced" | "upselling"
    pub invoice_status: String,
}

/// The order-level invoicing-policy read: the aggregate `invoice_status` (downpayment lines
/// EXCLUDED) over the per-line computes.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SalesOrderInvoiceStatusDto {
    pub order_id: uuid::Uuid,
    pub order_number: String,
    pub status: String,
    /// "no" | "to invoice" | "invoiced" | "upselling"
    pub invoice_status: String,
    pub lines: Vec<SalesOrderItemInvoiceDto>,
}

/// One quotation line's invoicing-policy read. Quotation lines carry no watermarks — this surfaces
/// the policy + downpayment flags conversion will carry onto the order lines.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotationItemInvoiceDto {
    pub id: uuid::Uuid,
    pub item_id: uuid::Uuid,
    /// "order" | "delivery"
    pub invoice_policy: String,
    pub is_downpayment: bool,
    pub quantity: rust_decimal::Decimal,
    /// `quantity − 0` pre-order: no watermarks exist before an order does.
    pub qty_to_invoice: rust_decimal::Decimal,
    /// Always "no" pre-order.
    pub invoice_status: String,
}

/// The quotation-level invoicing-policy read: per-line flags + the (always "no") aggregate —
/// nothing is invoiceable before an order exists.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotationInvoiceStatusDto {
    pub quotation_id: uuid::Uuid,
    pub quotation_number: String,
    pub status: String,
    pub invoice_status: String,
    pub lines: Vec<QuotationItemInvoiceDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }

    // Policy base: delivery lines bill on delivered_qty; downpayment lines stay on quantity.
    #[test]
    fn policy_base_follows_policy_and_downpayment() {
        assert_eq!(policy_base("order", false, d("7"), d("10")), d("10"));
        assert_eq!(policy_base("delivery", false, d("7"), d("10")), d("7"));
        assert_eq!(policy_base("delivery", true, d("7"), d("10")), d("10"));
        assert_eq!(policy_base("order", true, d("7"), d("10")), d("10"));
    }

    // Aggregate precedence: a to-invoice line outranks an upselling line (actionable-first).
    #[test]
    fn aggregate_is_actionable_first() {
        assert_eq!(aggregate_invoice_status(&["to invoice", "upselling", "invoiced"]), "to invoice");
        assert_eq!(aggregate_invoice_status(&["upselling", "invoiced"]), "upselling");
        assert_eq!(aggregate_invoice_status(&["invoiced", "invoiced"]), "invoiced");
        assert_eq!(aggregate_invoice_status(&["no"]), "no");
        assert_eq!(aggregate_invoice_status(&[]), "no");
    }
}
