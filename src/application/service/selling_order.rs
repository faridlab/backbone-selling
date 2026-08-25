//! Sales orders: create (direct + cart-priced), confirm, convert-from-quotation, cancel,
//! line edits under the order lock, ref lookup (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `create_sales_order` prices the basket server-side (2dp half-up) and writes header + lines as
//! ONE transaction. `create_sales_order_priced` is the promo CART seam (ADR-002) layered on top —
//! it resolves per-line nets via the `CartPricingPort` and maps them back to
//! `(unit_price, line_discount)` so the order's own `price_document` reproduces the cart total
//! exactly. Zero normal Cargo edge to promo. `confirm_sales_order` is the gated
//! draft → `to_deliver_and_bill` flip (council 2026-07-05; ADR-003); `convert_quotation_to_order`
//! is the quote→order step that copies header + lines (including each line's invoicing policy and
//! downpayment flag) and links back to the quotation; `cancel_sales_order` is the one-way exit —
//! refused the moment any line carries a billed quantity (posted invoices are never cancelled);
//! `update_order_line` is the order lock — frozen fields refuse once the order is confirmed;
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
use super::selling_events::{SalesOrderCancelled, SalesOrderConfirmed, SalesOrderRef, SellingEvent};
use super::selling_unit_cost::{ItemUnitCost, UnitCostPort, UnitCostRequest};
use super::selling_write_service::{
    is_dup, money, price_document, NewCartSalesOrder, NewLine, NewSalesOrder, SellingError,
    SellingWriteService,
};

/// One order-line edit. All-`None` = a no-op; `description` alone stays allowed on a confirmed
/// order (the label, not the commitment); any of item/qty/price/discount on a non-draft order is
/// the frozen-field refusal — confirmed demand is not silently re-priced.
#[derive(Debug, Clone, Default)]
pub struct UpdateOrderLinePatch {
    pub description: Option<String>,
    pub item_id: Option<Uuid>,
    pub quantity: Option<Decimal>,
    pub unit_price: Option<Decimal>,
    pub line_discount: Option<Decimal>,
}

impl UpdateOrderLinePatch {
    fn touches_frozen_fields(&self) -> bool {
        self.item_id.is_some() || self.quantity.is_some() || self.unit_price.is_some() || self.line_discount.is_some()
    }
}

impl SellingWriteService {
    pub async fn create_sales_order(&self, o: NewSalesOrder) -> Result<Uuid, SellingError> {
        let (priced, subtotal, tax_amount, total) = price_document(&o.lines, o.tax_rate)?;
        // A create-time carrier choice must name one of THIS company's carriers (a clean 404,
        // never the FK violation's 500) — validated before the transaction opens.
        let delivery_carrier_id = self
            .carrier_id_or_refuse(&o.company_id, o.delivery_carrier_id)
            .await?;
        let id = Uuid::new_v4();
        let currency = o.currency.unwrap_or_else(|| "IDR".into());
        // RLS scope (ADR-0008): bind the order's company onto the header+lines transaction.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, o.company_id).await?;
        let r = self.repos.orders.insert_draft(&mut tx, &NewSalesOrderRow {
            id,
            order_number: &o.order_number,
            quotation_id: o.quotation_id,
            delivery_carrier_id,
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
                invoice_policy: &p.invoice_policy.to_string(),
                is_downpayment: p.is_downpayment,
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
                invoice_policy: None,
                is_downpayment: None,
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
                invoice_policy: None,
                is_downpayment: None,
            });
        }

        self.create_sales_order(NewSalesOrder {
            order_number: o.order_number,
            quotation_id: None,
            delivery_carrier_id: None,
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
    ///
    /// Confirm a draft sales order (draft → to_deliver_and_bill); emits `SalesOrderConfirmed`.
    /// Since the unit-cost margin snapshot landed, confirm also STAMPS each live line's
    /// `unit_cost` from the `costs` port (the confirm-time snapshot — no later edit and no later
    /// catalog standard_cost change can rewrite it; the stamp statement is the only writer).
    ///
    /// Flow: (1) read the order's live (line id, item id) pairs on the request scope; (2) ask the
    /// port for the DISTINCT items' costs — BEFORE any transaction, so no network call runs inside
    /// the DB tx and draft lines are not locked across the port call; (3) in ONE transaction
    /// (company bound on it): stamp the snapshots, then run the UNCHANGED draft→confirmed guard.
    /// The guard losing (0 rows) rolls the stamp back with it — a loser of two concurrent confirms
    /// never leaves a cost on a non-confirmed order.
    ///
    /// Port-failure rule (explicit, tested): a port `Err`, a requested item MISSING from the
    /// response, or a NEGATIVE cost each REFUSE the confirm with `CostRejected` — the order stays
    /// draft, no event fires. A confirm is a commitment; an unknown-cost confirm corrupts margin
    /// analytics silently. A NULL cost for an item PROCEEDS (that line snapshots NULL — margin
    /// reads NULL, never zero). The refusal is not sticky: a retried confirm with a healthy port
    /// succeeds.
    ///
    /// Race note: a draft line ADDED between the (1) read and the (3) stamp is not in the stamp's
    /// unnest table and stays NULL — honest absence, never a WRONG cost, only a missing one
    /// (tightening would need `FOR UPDATE` line reads inside the tx; deliberately not taken in
    /// this release). The line-edit path's `FOR UPDATE` serializes its writes against the stamp.
    ///
    /// `company_id` scopes everything, so a principal of company A cannot confirm company B's
    /// order by knowing its id — a mismatched tenant reads no lines and loses the guard, which is
    /// indistinguishable from a missing order (`NotDraft`), so this does not leak whether the id
    /// exists.
    pub async fn confirm_sales_order(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        costs: &dyn UnitCostPort,
    ) -> Result<(), SellingError> {
        // (1) live (line, item) pairs — ID-only, company-scoped through the caller's fence; a
        // wrong-tenant order simply reads [] and the guard below refuses with NotDraft.
        let lines = company_scope::with_company_scope(
            Some(company_id),
            self.repos.order_items.list_cost_stamp_lines(&self.db_pool, order_id),
        ).await?;

        // (2) resolve the DISTINCT items' costs outside any transaction.
        let mut stamps: Vec<(Uuid, Option<Decimal>)> = Vec::with_capacity(lines.len());
        if !lines.is_empty() {
            let mut item_ids: Vec<Uuid> = lines.iter().map(|l| l.item_id).collect();
            item_ids.sort_unstable();
            item_ids.dedup();
            let resolved = costs
                .resolve_unit_costs(&UnitCostRequest { company_id, item_ids })
                .await
                .map_err(|e| SellingError::CostRejected { code: e.code, message: e.message })?;

            let cost_of = |item_id: Uuid| -> Result<Option<Decimal>, SellingError> {
                let entry: &ItemUnitCost = resolved
                    .iter()
                    .find(|c| c.item_id == item_id)
                    .ok_or_else(|| SellingError::CostRejected {
                        code: "unit_cost_line_missing".into(),
                        message: "cost source omitted an item".into(),
                    })?;
                match entry.unit_cost {
                    Some(c) if c < Decimal::ZERO => Err(SellingError::CostRejected {
                        code: "unit_cost_negative".into(),
                        message: "cost source returned a negative unit cost".into(),
                    }),
                    other => Ok(other),
                }
            };
            for l in &lines {
                stamps.push((l.id, cost_of(l.item_id)?));
            }
        }

        // (3) stamp + guard as ONE unit of work; a losing guard rolls the stamp back.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, company_id).await?;
        if !stamps.is_empty() {
            self.repos.order_items.stamp_unit_costs(&mut tx, order_id, &stamps).await?;
        }
        let row = self.repos.orders.confirm_tx(&mut tx, order_id, company_id).await?;
        let Some(row) = row else {
            // Refusal typing is byte-identical to the pre-snapshot era: wrong tenant, absent id,
            // and non-draft are ALL `NotDraft` — no existence leak.
            return Err(SellingError::NotDraft(order_id.to_string()));
        };
        tx.commit().await?;
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
            // The quotation line's invoicing policy + downpayment flag are preserved verbatim —
            // conversion must never change the billing intent the offer committed to.
            invoice_policy: l.invoice_policy.parse().ok(),
            is_downpayment: Some(l.is_downpayment),
        }).collect();

        let order_id = self.create_sales_order(NewSalesOrder {
            order_number,
            quotation_id: Some(quotation_id),
            delivery_carrier_id: None,
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

    /// Cancel a sales order (draft/to_deliver/to_bill/to_deliver_and_bill → cancelled); emits
    /// `SalesOrderCancelled`. Refused — loudly — when any live line carries a billed quantity
    /// (`order_billed`): posted invoices are never cancelled, credit notes are the correction
    /// path. The billed check and the flip are ONE atomic statement, so a racing `mark_invoiced`
    /// cannot slip a billed quantity between check and flip. A delivered-but-unbilled order CAN be
    /// cancelled (only billed guards; delivery reversal is inventory's lane).
    pub async fn cancel_sales_order(
        &self,
        order_id: Uuid,
        company_id: Uuid,
    ) -> Result<(), SellingError> {
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.orders.cancel(&self.db_pool, order_id, company_id),
        ).await?;
        let row = match row {
            Some(r) => r,
            None => {
                // The guard refused — classify why (only after a refusal; the guarded statement
                // itself never leaks whether the id exists).
                let why = company_scope::with_company_scope(
                    Some(company_id),
                    self.repos.orders.find_cancel_refusal(&self.db_pool, order_id, company_id),
                ).await?;
                return Err(match why {
                    None => SellingError::OrderNotFound(order_id),
                    // A terminal order refuses because it is terminal; an in-flight order with a
                    // billed line refuses because posted invoices are never cancelled.
                    Some(r) if matches!(r.status.as_str(), "completed" | "closed" | "cancelled") => {
                        SellingError::InvalidTransition { verb: "cancel".into(), current: r.status }
                    }
                    Some(r) if r.has_billed => SellingError::OrderBilled,
                    Some(r) => SellingError::InvalidTransition {
                        verb: "cancel".into(),
                        current: r.status,
                    },
                });
            }
        };
        self.sink.publish(SellingEvent::SalesOrderCancelled(SalesOrderCancelled {
            order_id,
            company_id: row.company_id,
            customer_id: row.customer_id,
        }));
        Ok(())
    }

    /// Edit one order line under the ORDER LOCK. Once the order's status is anything other than
    /// `draft`, the frozen fields (item, quantity, unit price, discount) refuse with
    /// `order_line_frozen` — only the description may still change. On a draft, a priced-field
    /// edit re-prices the line (`money(qty*price) − money(discount)`) and re-derives the header's
    /// subtotal/tax/total from the full live line set in the SAME transaction. An all-`None` patch
    /// is a no-op. The line + its parent header are read under `FOR UPDATE`, so the freeze check
    /// cannot race a concurrent `confirm_sales_order`.
    pub async fn update_order_line(
        &self,
        line_id: Uuid,
        company_id: Uuid,
        patch: UpdateOrderLinePatch,
    ) -> Result<(), SellingError> {
        if patch.description.is_none() && !patch.touches_frozen_fields() {
            return Ok(()); // nothing asked, nothing changed
        }
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, company_id).await?;
        let line = self.repos.order_items
            .lock_line_with_parent_status(&mut tx, line_id, company_id).await?
            .ok_or(SellingError::OrderNotFound(line_id))?;

        if line.order_status != "draft" && patch.touches_frozen_fields() {
            return Err(SellingError::OrderLineFrozen);
        }

        let quantity = patch.quantity.unwrap_or(line.quantity);
        let unit_price = patch.unit_price.unwrap_or(line.unit_price);
        let line_discount = patch.line_discount.unwrap_or(line.line_discount);
        let priced_changed = patch.quantity.is_some() || patch.unit_price.is_some() || patch.line_discount.is_some();

        let line_amount = if priced_changed {
            if quantity < Decimal::ZERO || unit_price < Decimal::ZERO || line_discount < Decimal::ZERO {
                return Err(SellingError::NegativeQuantity);
            }
            let gross = money(quantity * unit_price);
            let amount = gross - money(line_discount);
            if amount < Decimal::ZERO {
                return Err(SellingError::NegativeQuantity);
            }
            amount
        } else {
            // description-only edit: keep the stored amount untouched
            line.line_amount
        };

        self.repos.order_items.update_line_full(
            &mut tx,
            line_id,
            patch.description.as_deref().or(line.description.as_deref()),
            patch.item_id.unwrap_or(line.item_id),
            quantity,
            unit_price,
            line_discount,
            line_amount,
        ).await?;

        if priced_changed {
            self.repos.orders.recompute_totals_from_lines(&mut tx, line.order_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
