//! Sales orders: create (direct + cart-priced), confirm, convert-from-quotation, cancel,
//! line edits under the order lock, ref lookup (hand-authored, user-owned).
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! `create_sales_order` prices the basket server-side (2dp half-up) and writes header + lines as
//! ONE transaction. `create_sales_order_priced` is the promo CART seam (ADR-002) layered on top —
//! it resolves per-line nets via the `CartPricingPort` and maps them back to
//! `(unit_price, line_discount)` so the order's own `price_document` reproduces the cart total
//! exactly. Zero normal Cargo edge to promo. `confirm_sales_order` is the gated
//! draft → `to_deliver_and_bill` flip (council 2026-07-05; ADR-003) — it stamps the confirm-time
//! unit-cost snapshot AND launches the stock rules for every stock-tracked line through the
//! [`super::selling_stock_fulfillment`] port (the sale_stock confirm engine) AND mints the
//! project/task delivery work for every service-tracked line through the
//! [`super::selling_service_delivery`] port (its policy read rides
//! [`super::selling_service_catalog`]); `convert_quotation_to_order`
//! is the quote→order step that copies header + lines (including each line's invoicing policy and
//! downpayment flag) and links back to the quotation; `cancel_sales_order` is the one-way exit —
//! refused the moment any line carries a billed quantity (posted invoices are never cancelled) —
//! and on success logs decrease-quantity activities UPSTREAM through the same port instead of
//! silently un-reserving; `update_order_line` is the order lock — frozen fields refuse
//! once the order is confirmed; `sales_order_ref` loads the cross-module DTO.
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
use super::selling_service_catalog::{ServiceCatalogPort, ServiceTrackingInfo, ServiceTrackingRung};
use super::selling_service_delivery::{ProjectFulfillmentPort, ServiceDeliveryLine, ServiceDeliveryRequest};
use super::selling_stock_fulfillment::{
    DecreaseQuantityLine, DecreaseQuantityRequest, StockFulfillmentPort, StockRuleLine,
    StockRuleRequest,
};
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
            delivery_carrier_id: o.delivery_carrier_id,
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
    /// Since the sale_stock confirm engine landed, confirm also LAUNCHES the stock rules for
    /// every stock-tracked line through the `stock` port (the procurement-group → rule → move →
    /// picking intent; see [`super::selling_stock_fulfillment`]).
    ///
    /// Flow: (1) read the order's live (line id, item id) pairs on the request scope; (2) ask the
    /// cost port for the DISTINCT items' costs — BEFORE any transaction, so no network call runs
    /// inside the DB tx and draft lines are not locked across the port call; (2b) ask the stock
    /// port to launch the order's non-downpayment lines (the port launches only stock-tracked
    /// items; a service line is a skip, not an error) — also before any transaction; (3) in ONE
    /// transaction (company bound on it): stamp the snapshots, then run the UNCHANGED
    /// draft→confirmed guard. The guard losing (0 rows) rolls the stamp back with it — a loser of
    /// two concurrent confirms never leaves a cost on a non-confirmed order.
    ///
    /// Port-failure rule (explicit, tested): a cost-port `Err`, a requested item MISSING from the
    /// response, or a NEGATIVE cost each REFUSE the confirm with `CostRejected` — the order stays
    /// draft, no event fires. A stock-port `Err` REFUSES the confirm with `FulfillmentRejected`
    /// for the same reason: a confirm is a commitment, and a confirmed order whose fulfillment
    /// silently never launched is corrupt. A NULL cost for an item PROCEEDS (that line snapshots
    /// NULL — margin reads NULL, never zero). Neither refusal is sticky: a retried confirm with
    /// healthy ports succeeds.
    ///
    /// Launch-before-commit note: the stock port runs BEFORE the confirm transaction commits
    /// (the port writes the stock engine's own tables and cannot join selling's transaction), so
    /// the port's idempotency-per-order contract is what makes a concurrent duplicate confirm or
    /// a retry after a lost guard race safe — the second launch returns the first's outcomes
    /// instead of double-minting moves. In the crash window where the launch landed but the
    /// confirm transaction failed, the moves exist for a still-draft order; the retried confirm
    /// re-launches (a no-op per the contract) and commits. An outcome for a line the confirm did
    /// not launch this call (already launched, or not stock-tracked) is accepted verbatim — the
    /// port is the record of what launched.
    ///
    /// Service-delivery note: since the service-delivery confirm engine landed, confirm ALSO
    /// resolves each non-downpayment line's product service-tracking policy through the
    /// `catalog` port and asks the `delivery` port to MINT the project/task work the confirmed
    /// line commits to — same before-the-transaction posture as the stock launch, same
    /// fail-closed refusal (`ServiceCatalogRejected` / `ServiceDeliveryRejected`), and the
    /// mint's per-sale-line idempotency covers the same crash window. What the mint reports is
    /// stamped onto the order lines (`project_id` / `task_id`) INSIDE the confirm transaction —
    /// the backrefs are selling's only record of the mint. A product ABSENT from the catalog
    /// resolution is the manual policy (mints nothing) — absence is a configuration, not a
    /// refusal; only the port's `Err` refuses.
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
        stock: &dyn StockFulfillmentPort,
        catalog: &dyn ServiceCatalogPort,
        delivery: &dyn ProjectFulfillmentPort,
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

        // (2b) launch the stock rules for the order's non-downpayment lines, outside any
        // transaction. A refused launch refuses the whole confirm — the order stays draft with
        // no stamp written (the stamp only happens inside the transaction below) and no event
        // fired. Downpayment lines are excluded: a downpayment's placeholder quantity is never
        // physically delivered, so it never drives stock work.
        let stock_header = company_scope::with_company_scope(
            Some(company_id),
            self.repos.orders.find_stock_header(&self.db_pool, order_id),
        ).await?;
        if let Some(hdr) = stock_header {
            let demand_lines = company_scope::with_company_scope(
                Some(company_id),
                self.repos.order_items.list_stock_demand_lines(&self.db_pool, order_id),
            ).await?;
            if !demand_lines.is_empty() {
                let outcomes = stock
                    .launch_stock_rules(&StockRuleRequest {
                        order_id,
                        company_id: hdr.company_id,
                        customer_id: hdr.customer_id,
                        order_number: hdr.order_number.clone(),
                        lines: demand_lines
                            .iter()
                            .map(|l| StockRuleLine {
                                line_id: l.id,
                                item_id: l.item_id,
                                quantity: l.quantity,
                            })
                            .collect(),
                    })
                    .await
                    .map_err(|e| SellingError::FulfillmentRejected { code: e.code, message: e.message })?;
                // Observability only — the moves and their picking projections are the stock
                // engine's records; selling persists nothing about them.
                for o in &outcomes {
                    if o.launched {
                        tracing::debug!(
                            target: "selling.stock",
                            order_id = %order_id,
                            line_id = %o.line_id,
                            move_id = ?o.move_id,
                            picking_id = ?o.picking_id,
                            procure_method = ?o.procure_method,
                            "confirm launched stock rule"
                        );
                    }
                }
            }
        }
        // A missing stock header (wrong tenant / absent id) is NOT refused here: the guard
        // below is the sole authority on whether this order is confirmable, and its NotDraft
        // refusal types every absent-id case identically (no existence leak).

        // (2c) resolve the service-tracking policy for the order's non-downpayment products and
        // mint the delivery work a confirm commits to — outside any transaction, same posture as
        // the stock launch. A refused resolution or mint refuses the whole confirm (the order
        // stays draft, no stamp written, no event fired); a product ABSENT from the resolution
        // is the manual policy (mints nothing, proceeds). Downpayment lines are excluded: a
        // downpayment's placeholder quantity is never delivered, so it never drives delivery work.
        let mut backrefs: Vec<(Uuid, Option<Uuid>, Option<Uuid>)> = Vec::new();
        if let Some(hdr) = company_scope::with_company_scope(
            Some(company_id),
            self.repos.orders.find_delivery_header(&self.db_pool, order_id),
        ).await? {
            let service_lines = company_scope::with_company_scope(
                Some(company_id),
                self.repos.order_items.list_service_delivery_lines(&self.db_pool, order_id),
            ).await?;
            if !service_lines.is_empty() {
                let mut item_ids: Vec<Uuid> = service_lines.iter().map(|l| l.item_id).collect();
                item_ids.sort_unstable();
                item_ids.dedup();
                let policies = catalog
                    .resolve_service_tracking(company_id, &item_ids)
                    .await
                    .map_err(|e| SellingError::ServiceCatalogRejected { code: e.code, message: e.message })?;
                // Absent item = manual: the product surface holds no tracking row, which is
                // exactly what "tracked by hand" means. Not an error (contrast CostRejected).
                let policy_of = |item_id: Uuid| -> ServiceTrackingInfo {
                    policies
                        .iter()
                        .find(|p| p.item_id == item_id)
                        .cloned()
                        .unwrap_or(ServiceTrackingInfo {
                            item_id,
                            service_tracking: ServiceTrackingRung::Manual,
                            service_project_id: None,
                            service_project_template_id: None,
                        })
                };
                let outcomes = delivery
                    .mint_service_delivery(&ServiceDeliveryRequest {
                        order_id,
                        company_id: hdr.company_id,
                        customer_id: hdr.customer_id,
                        order_number: hdr.order_number.clone(),
                        currency: hdr.currency.clone(),
                        lines: service_lines
                            .iter()
                            .map(|l| {
                                let p = policy_of(l.item_id);
                                ServiceDeliveryLine {
                                    sale_line_id: l.id,
                                    item_id: l.item_id,
                                    quantity: l.quantity,
                                    description: l.description.clone(),
                                    rung: p.service_tracking,
                                    fixed_project_id: p.service_project_id,
                                    template_id: p.service_project_template_id,
                                }
                            })
                            .collect(),
                    })
                    .await
                    .map_err(|e| SellingError::ServiceDeliveryRejected { code: e.code, message: e.message })?;
                // The mint's outcomes are the record: only `minted: true` lines get a backref
                // (a manual or untracked line stamps nothing and keeps NULL).
                for o in &outcomes {
                    if o.minted {
                        backrefs.push((o.sale_line_id, o.project_id, o.task_id));
                    }
                }
            }
        }
        // A missing delivery header is the same non-refusal as the stock header above: the
        // guard below is the sole authority (NotDraft types every absent-id case identically).

        // (3) stamp + guard as ONE unit of work; a losing guard rolls the stamp back.
        let mut tx = self.db_pool.begin().await?;
        company_scope::bind_company_on(&mut tx, company_id).await?;
        if !stamps.is_empty() {
            self.repos.order_items.stamp_unit_costs(&mut tx, order_id, &stamps).await?;
        }
        if !backrefs.is_empty() {
            self.repos.order_items.stamp_service_backrefs(&mut tx, order_id, &backrefs).await?;
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
    ///
    /// Since the sale_stock confirm engine landed, a successful cancel also asks the `stock` port
    /// to LOG DECREASE-QUANTITY ACTIVITIES on the upstream fulfillment records (the pickings and
    /// moves the confirm launched) instead of silently un-reserving: selling holds no
    /// reservation of its own to release — reservations live on the stock engine's quants — so
    /// the activity log is the loud channel that tells the stock side a confirmed demand went
    /// away, and an operator decides what to do with any already-reserved or already-shipped
    /// quantity.
    ///
    /// Ordering + failure posture (explicit, tested): the log is requested only AFTER the guarded
    /// flip committed — logging for an order that then refuses cancellation would tell the stock
    /// side to decrease quantities the order still stands behind, which is worse than a missing
    /// log. The port sits outside selling's transaction, so a log failure cannot roll the
    /// cancellation back: the order IS cancelled, the `SalesOrderCancelled` event still fires
    /// (consumers must see the commitment's end), and the method returns
    /// `DecreaseActivityFailed` telling the caller to re-invoke
    /// [`Self::retry_decrease_activities`] with a healthy engine. Never a silent skip.
    pub async fn cancel_sales_order(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        stock: &dyn StockFulfillmentPort,
    ) -> Result<(), SellingError> {
        let row = company_scope::with_company_scope(
            Some(company_id),
            self.repos.orders.cancel(&self.db_pool, order_id, company_id),
        ).await?;
        let row = match row {
            Some(r) => r,
            None => {
                // The guard refused — classify why (only after a refusal; the guarded statement
                // itself never leaks whether the id exists). No port call happens on a refusal:
                // nothing was cancelled, so there is nothing to tell the stock side.
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
        // The commitment's end is published first (it happened); the upstream log follows. A
        // failure here is returned loudly but does not undo anything — see the method doc.
        self.log_decrease_activities(order_id, company_id, &row.order_number, stock).await?;
        Ok(())
    }

    /// Re-attempt the upstream decrease-quantity log for an order whose cancellation committed
    /// but whose log call failed (`DecreaseActivityFailed`). Refuses unless the order actually
    /// IS cancelled (the log is only ever about a cancelled demand). The port's
    /// idempotency-per-order contract makes a retry after an ambiguous failure safe.
    pub async fn retry_decrease_activities(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        stock: &dyn StockFulfillmentPort,
    ) -> Result<(), SellingError> {
        let hdr = company_scope::with_company_scope(
            Some(company_id),
            self.repos.orders.find_stock_header(&self.db_pool, order_id),
        ).await?
        .ok_or(SellingError::OrderNotFound(order_id))?;
        if hdr.status != "cancelled" {
            return Err(SellingError::InvalidTransition { verb: "retry_decrease_activities".into(), current: hdr.status });
        }
        self.log_decrease_activities(order_id, company_id, &hdr.order_number, stock).await
    }

    /// Build and send the decrease-quantity request for a cancelled order: one entry per live
    /// non-downpayment line, carrying what was ordered vs what had shipped (the stored delivery
    /// watermark at cancel time). Shared by the cancel flow and its retry verb.
    async fn log_decrease_activities(
        &self,
        order_id: Uuid,
        company_id: Uuid,
        order_number: &str,
        stock: &dyn StockFulfillmentPort,
    ) -> Result<(), SellingError> {
        let lines = company_scope::with_company_scope(
            Some(company_id),
            self.repos.order_items.list_stock_demand_lines(&self.db_pool, order_id),
        ).await?;
        if lines.is_empty() {
            return Ok(()); // nothing was ever orderable — nothing to decrease upstream
        }
        stock
            .log_decrease_quantity(&DecreaseQuantityRequest {
                order_id,
                company_id,
                order_number: order_number.to_string(),
                lines: lines
                    .iter()
                    .map(|l| DecreaseQuantityLine {
                        line_id: l.id,
                        item_id: l.item_id,
                        ordered_qty: l.quantity,
                        delivered_qty: l.delivered_qty,
                    })
                    .collect(),
            })
            .await
            .map_err(|e| SellingError::DecreaseActivityFailed { code: e.code, message: e.message })
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
