//! The margin engine (hand-authored, user-owned) — PURE COMPUTE over the confirm-time
//! unit-cost snapshots, single source.
//!
//! A line's `unit_cost` is stamped ONCE, by the confirm flow, from the catalog's standard cost
//! through the `UnitCostPort` (the Odoo `purchase_price` shape). The canonical margin expression
//! is ONE formula, mirrored in the SQL rollup (`margin_rollup` on `SalesOrderItemRepository`)
//! and here:
//!
//! ```text
//! line_margin(line)   = line_amount − unit_cost · quantity    // costed lines only
//! margin_percent(line) = round2( line_margin / line_amount × 100 )   // None ⇔ line_amount = 0
//! ```
//!
//! Total-basis note: `line_amount − unit_cost·qty` is algebraically
//! `(net price_unit − unit_cost)·qty` with `net price_unit = line_amount/qty`, but computed on
//! the PERSISTED 2dp `line_amount` it avoids a second rounding step — the same figure the
//! invoice/billing surface bills against.
//!
//! Keeping the expression identical in both places is the invariant this file documents: drift
//! makes the order rollup and the per-line computes contradict each other.
//!
//! ## NULL-cost semantics (honest absence — the rule that overrides every default)
//!
//! `unit_cost` NULL ⇒ `margin` NULL and `margin_percent` NULL. NEVER zero: zero is a REAL
//! zero-margin trade (goods given at cost), and conflating "no cost maintained" with "sold at
//! cost" corrupts margin analytics silently. A NULL-stamped cost is indistinguishable in the
//! column from a never-stamped one — deliberately: both mean "unknown", and no marker column
//! distinguishes them (a documented non-goal of the confirm stamp).
//!
//! ## Negative margins are legal
//!
//! `cost > price` computes a negative margin — a real loss on the line, reported as such.
//! Free promo reward lines (zero-priced buy-X-get-Y goods) compute `−cost·qty` BY DESIGN; the
//! order's cart total was already adjusted when the reward line was added, so the margin view
//! must reflect the giveaway's cost. Downpayment lines are treated uniformly with goods lines
//! (their snapshot is whatever the port resolved at confirm).
//!
//! ## Read-time only
//!
//! `margin` / `margin_percent` are COMPUTED at read time and exposed on the margin read DTO —
//! they are never persisted and no write route accepts them (structurally: they are not schema
//! fields; same guarantee as `qty_to_invoice` in `selling_invoice_policy.rs`). Rounding happens
//! HERE, at the DTO surface only (2dp half-up via the write service's `money` helper); the SQL
//! mirror stays full-precision.
//!
//! ## Returns/credits are NOT netted
//!
//! Margin is the CONFIRMED-LINE margin: `billed_qty` / `delivered_qty` never enter the
//! expression, and billing's credit notes (the returns path — selling exited invoices, ADR-006)
//! do not net out of this read. A cancelled order's lines KEEP their snapshots and the view
//! keeps computing from them (a never-realized estimate — the view is a read; refusing it has
//! no integrity value). Return-netting needs a return-side watermark selling does not own;
//! registered as a refinement for a later wave.
//!
//! Per the module's 4-layer rule this file holds no SQL — the reads live on
//! `SalesOrderItemRepository` / `SalesOrderRepository`.

use rust_decimal::Decimal;
use uuid::Uuid;

use super::selling_write_service::{money, SellingError, SellingWriteService};

/// `line_margin = line_amount − unit_cost·quantity`. Call only when `unit_cost` is `Some`;
/// RAW (not rounded) — round via `money` at the DTO surface only.
pub fn line_margin(line_amount: Decimal, unit_cost: Decimal, quantity: Decimal) -> Decimal {
    line_amount - unit_cost * quantity
}

/// `margin_percent = round2(line_margin / line_amount × 100)`. `None` ⇔ `line_amount == 0`
/// (a zero-amount costed line has no meaningful percentage — report NULL, not a divide panic).
pub fn margin_percent(line_margin: Decimal, line_amount: Decimal) -> Option<Decimal> {
    if line_amount == Decimal::ZERO {
        None
    } else {
        Some(money(line_margin / line_amount * Decimal::from(100)))
    }
}

impl SellingWriteService {
    /// The order margin read model: per-line margin / margin_percent computes over the
    /// confirm-time unit-cost snapshots, plus the order rollup (costed subset only) and the
    /// coverage counters that make partial-coverage orders visible. Pure compute at read time —
    /// nothing here is stored or writable. ID-only read (rides the caller's request-scoped
    /// connection; RLS fences it).
    pub async fn order_margin_view(&self, order_id: Uuid) -> Result<SalesOrderMarginDto, SellingError> {
        let hdr = self.repos.orders.find_invoice_status_header(&self.db_pool, order_id).await?
            .ok_or(SellingError::OrderNotFound(order_id))?;
        let rows = self.repos.order_items.list_margin_rows(&self.db_pool, order_id).await?;
        let rollup = self.repos.order_items.margin_rollup(&self.db_pool, order_id).await?;

        let lines: Vec<SalesOrderMarginLineDto> = rows.iter().map(|r| {
            // NULL cost ⇒ NULL margin + NULL percent (honest absence, never zero).
            let (margin, percent) = match r.unit_cost {
                None => (None, None),
                Some(c) => {
                    let m = line_margin(r.line_amount, c, r.quantity);
                    (Some(money(m)), margin_percent(m, r.line_amount))
                }
            };
            SalesOrderMarginLineDto {
                id: r.id,
                item_id: r.item_id,
                quantity: r.quantity,
                unit_price: r.unit_price,
                line_discount: r.line_discount,
                line_amount: r.line_amount,
                unit_cost: r.unit_cost,
                margin,
                margin_percent: percent,
            }
        }).collect();

        // Order rollup over the COSTED subset: None when no line carries a snapshot (the whole
        // order is unknown-cost), Some — even 0.00 — when at least one line does. Percent is
        // over the costed amount sum only, so an uncosted line never dilutes the ratio.
        let order_margin = rollup.margin_sum.map(money);
        let order_margin_percent = match (order_margin, rollup.amount_sum_costed) {
            (Some(m), Some(a)) if a != Decimal::ZERO => margin_percent(m, a),
            _ => None,
        };

        Ok(SalesOrderMarginDto {
            order_id,
            order_number: hdr.order_number,
            status: hdr.status,
            order_margin,
            order_margin_percent,
            margin_lines_costed: rollup.costed_lines,
            margin_lines_total: rollup.total_lines,
            lines,
        })
    }
}

// ── the margin read DTOs ───────────────────────────────────────────────────────
//
// These live HERE (the user-owned margin file), not in the generated `presentation/dto/*.rs`
// files: a schema regen rewrites those files wholesale and eats hand-appended blocks. The
// margin engine owns these shapes outright; nothing generator-owned references them.

/// One order line's margin read: the persisted snapshot figures plus the COMPUTED
/// `margin` / `marginPercent` pair (read-time only — never persisted, no write route can set
/// them). `unitCost`/`margin`/`marginPercent` are null when no cost was maintained.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SalesOrderMarginLineDto {
    pub id: uuid::Uuid,
    pub item_id: uuid::Uuid,
    pub quantity: rust_decimal::Decimal,
    pub unit_price: rust_decimal::Decimal,
    pub line_discount: rust_decimal::Decimal,
    pub line_amount: rust_decimal::Decimal,
    /// The confirm-time cost-per-unit snapshot (18,6). NULL = no cost maintained.
    pub unit_cost: Option<rust_decimal::Decimal>,
    /// `line_amount − unit_cost·quantity`, 2dp. NULL ⇔ `unit_cost` NULL (never zero by default).
    /// Negative values are legal (cost > price; free reward lines are negative by design).
    pub margin: Option<rust_decimal::Decimal>,
    /// `margin / line_amount × 100`, 2dp. NULL ⇔ margin NULL or `line_amount == 0`.
    pub margin_percent: Option<rust_decimal::Decimal>,
}

/// The order-level margin read: the rollup over the COSTED lines plus the coverage counters
/// (`marginLinesCosted` / `marginLinesTotal` make partial-coverage orders visible — analytics
/// consumers must treat NULL as "unknown", never zero).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SalesOrderMarginDto {
    pub order_id: uuid::Uuid,
    pub order_number: String,
    pub status: String,
    /// `Σ(line_amount − unit_cost·qty)` over costed lines, 2dp. NULL when no line carries a
    /// snapshot; `Some(0.00)` when a costed subset nets to exactly zero.
    pub order_margin: Option<rust_decimal::Decimal>,
    /// `orderMargin / Σ costed line_amount × 100`, 2dp. NULL when `orderMargin` is NULL or the
    /// costed amount sum is zero.
    pub order_margin_percent: Option<rust_decimal::Decimal>,
    pub margin_lines_costed: i64,
    pub margin_lines_total: i64,
    pub lines: Vec<SalesOrderMarginLineDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal { Decimal::from_str_exact(s).unwrap() }

    // line_margin = line_amount − cost·qty (total basis — no second rounding step).
    #[test]
    fn line_margin_is_amount_minus_cost_times_qty() {
        assert_eq!(line_margin(d("1000.00"), d("600.00"), d("2")), d("1000.00") - d("1200.00"));
        assert_eq!(line_margin(d("1000.00"), d("400.00"), d("2")), d("200.00"));
    }

    // A free-goods line (zero price) computes the FULL cost as a negative margin — by design.
    #[test]
    fn free_goods_line_margin_is_negative_cost() {
        assert_eq!(line_margin(d("0.00"), d("150.00"), d("3")), d("-450.00"));
    }

    // margin_percent: 2dp, None on zero amount.
    #[test]
    fn margin_percent_rounds_and_nones_on_zero() {
        assert_eq!(margin_percent(d("250.00"), d("1000.00")), Some(d("25.00")));
        assert_eq!(margin_percent(d("-50.00"), d("1000.00")), Some(d("-5.00")));
        assert_eq!(margin_percent(d("1.00"), d("0.00")), None);
    }
}
