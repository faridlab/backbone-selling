//! Sales orders: create (direct + cart-priced), confirm, convert-from-quotation, ref lookup
//! (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `create_sales_order` prices the basket server-side (2dp half-up) and writes header + lines as
//! ONE transaction. `create_sales_order_priced` is the promo CART seam (ADR-002) layered on top —
//! it resolves per-line nets via the `CartPricingPort` and maps them back to
//! `(unit_price, line_discount)` so the order's own `price_document` reproduces the cart total
//! exactly. Zero normal Cargo edge to promo. `confirm_sales_order` is the gated
//! draft → `to_deliver_and_bill` flip (council 2026-07-05; ADR-003); `convert_quotation_to_order`
//! is the quote→order step that copies header + lines and links back to the quotation;
//! `sales_order_ref` loads the cross-module DTO.
//!
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `SalesOrderRepository` / `SalesOrderItemRepository` (and `QuotationRepository` /
//! `QuotationItemRepository` for the conversion read).

use backbone_orm::company_scope;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::infrastructure::persistence::{NewSalesOrderItemRow, NewSalesOrderRow};

use super::selling_cart_pricing::{CartPriceLine, CartPriceRequest, CartPricingPort};
use super::selling_events::{SalesOrderConfirmed, SalesOrderRef, SellingEvent};
use super::selling_write_service::{
    is_dup, money, price_document, NewCartSalesOrder, NewLine, NewSalesOrder, SellingError,
    SellingWriteService,
};

impl SellingWriteService {
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
}
