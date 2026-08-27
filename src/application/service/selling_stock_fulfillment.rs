//! Outbound stock-fulfillment port (hand-authored, user-owned) — selling's side of the
//! sale_stock confirm engine.
//!
//! Confirming a sales order COMMITS demand. For every line whose item is stock-tracked
//! ("storable" in the product-type vocabulary) that commitment becomes physical work: a
//! procurement demand the stock engine resolves through its route rules into a draft move,
//! which a picking projects. Selling does not — and must not — know how routes are selected,
//! how moves chain, or how pickings re-project: that is the inventory module's vocabulary.
//! This file holds only the `StockFulfillmentPort` trait + selling's own DTOs; a composing
//! service implements the port over its stock engine. **Zero normal Cargo edge** to
//! inventory — the DTOs are the wire contract, duplicated per consumer by design (same
//! posture as [`super::selling_unit_cost`]'s `UnitCostPort`).
//!
//! The port models the confirm intent on the stock engine's own vocabulary:
//!
//! - **procurement group** — the request as a whole: one confirmed order is one demand
//!   group (`order_id` + `order_number` are the group's identity and correspondence keys).
//! - **rule** — resolved host-side per line: the port's implementation selects the route
//!   rule that covers the demand location (the "highest-sequence active pull rule"
//!   ordering). Selling never sees the rule.
//! - **move** — minted per storable line at the requested quantity; the outcome carries
//!   the move id so the composition can observe what was launched.
//! - **picking** — a PROJECTION of moves in the stock engine; the outcome carries the
//!   projected picking id when one already exists.
//!
//! Idempotency contract (the load-bearing rule): `launch_stock_rules` MUST be idempotent
//! per `order_id` — a repeated launch for a line that already launched returns the prior
//! outcome instead of minting again. Selling calls the port BEFORE its confirm transaction
//! commits (mirroring how the confirm-time unit-cost resolution works), so a concurrent
//! duplicate confirm or a confirm retried after a lost guard race must not double-mint
//! moves. Implementations typically key the moves' origin correspondence on
//! `SO/{order_number}` (or the line ids) and skip already-launched lines.
//!
//! The reconstruction read (`delivered_quantities`) is the inbound half: the physical
//! truth of what shipped. The line's delivered watermark is RECONSTRUCTED from the done
//! moves — gross outgoing, minus returns flagged `to_refund`. A return that is a straight
//! exchange (`to_refund` false) shipped a replacement and does not reduce the delivered
//! commitment; only a refund-shaped return does. Selling applies that return policy
//! itself (the port supplies the raw figures) because the policy is selling's, not the
//! warehouse's.
//!
//! The cancellation hook (`log_decrease_quantity`) follows a deliberate posture: when a
//! confirmed order is cancelled, selling does NOT silently un-reserve stock it cannot see —
//! reservations live on the stock engine's quants. Instead it asks the port to log
//! "decrease ordered quantity" activities on the upstream fulfillment records (the
//! pickings/moves the launch minted), where an operator sees them and acts.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// --- confirm: launch stock rules per storable line ----------------------------

/// One line's procurement demand — the sale-side expression of a procurement-group entry.
/// Downpayment lines never appear here (a downpayment's placeholder quantity is never
/// physically delivered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StockRuleLine {
    /// The selling order line — the port's implementation keys the move correspondence
    /// (origin) on the line or the order so a re-launch can find its prior work.
    pub line_id: Uuid,
    pub item_id: Uuid,
    pub quantity: Decimal,
}

/// The demand group a confirm emits: one confirmed order, one launch request. The
/// implementation decides per line whether the item is stock-tracked; non-storable items
/// are skipped (`launched: false`), not errors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StockRuleRequest {
    pub order_id: Uuid,
    pub company_id: Uuid,
    pub customer_id: Uuid,
    /// The order number is the group's human correspondence key — moves minted for this
    /// order carry it (or a derivative) in their origin, which is how the reconstruction
    /// read and the decrease-quantity log find them again.
    pub order_number: String,
    pub lines: Vec<StockRuleLine>,
}

/// What the launch did for one line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StockRuleOutcome {
    pub line_id: Uuid,
    /// `false` = the item is not stock-tracked (service/consumable) — nothing to launch,
    /// not a failure. The confirm proceeds.
    pub launched: bool,
    /// The minted draft move (the picking projects from moves; both live in the stock
    /// engine). `None` when `launched` is false.
    pub move_id: Option<Uuid>,
    /// The picking that already projects the move, when one does. `None` when `launched`
    /// is false or the engine has not projected one yet.
    pub picking_id: Option<Uuid>,
    /// The supply vocabulary the launch resolved for the line ("make_to_stock" /
    /// "make_to_order" / "mts_else_mto") — an observability echo, not a behavior switch.
    pub procure_method: Option<String>,
}

// --- inbound: qty_delivered reconstruction from moves --------------------------

/// One line the reconstruction asks about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredQtyLineRef {
    pub line_id: Uuid,
    pub item_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredQtyRequest {
    pub company_id: Uuid,
    pub order_id: Uuid,
    pub lines: Vec<DeliveredQtyLineRef>,
}

/// The raw move-backed figures for one line. Selling derives the reconstruction as
/// `delivered_qty − to_refund_qty`; a returned-but-exchanged quantity (the
/// `returned_qty − to_refund_qty` remainder) does not reduce it.
///
/// A line ABSENT from the response means "no move-backed figure for this line" (no stock
/// engine composed, or no moves were ever minted) — an honest absence the caller treats
/// as "keep the stored watermark", NEVER as zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MoveDeliveryFigures {
    pub line_id: Uuid,
    /// DONE outgoing moves against the line, gross.
    pub delivered_qty: Decimal,
    /// DONE incoming returns against the line, all of them (refund-shaped or exchange).
    pub returned_qty: Decimal,
    /// The returned subset flagged to-refund — the only part that reduces the delivered
    /// commitment.
    pub to_refund_qty: Decimal,
}

// --- cancellation: decrease-quantity activities upstream ------------------------

/// One line of the decrease-quantity log request: what was ordered vs what had shipped
/// when the order was cancelled. The upstream activity tells an operator exactly how much
/// ordered quantity to take back out of the fulfillment pipeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecreaseQuantityLine {
    pub line_id: Uuid,
    pub item_id: Uuid,
    pub ordered_qty: Decimal,
    /// The line's delivered watermark at cancel time (0 for a cancel before any ship).
    pub delivered_qty: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DecreaseQuantityRequest {
    pub order_id: Uuid,
    pub company_id: Uuid,
    pub order_number: String,
    pub lines: Vec<DecreaseQuantityLine>,
}

// --- error + port --------------------------------------------------------------

/// The stock engine's refusal (transport failure, no route covers the demand, ...).
/// Flat `{code, message}` by design — the implementing engine's own error taxonomy stays
/// on its side of the port (same shape as [`super::selling_unit_cost::UnitCostError`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StockFulfillmentError {
    pub code: String,
    pub message: String,
}

/// The stock-fulfillment seam selling's confirm and cancel depend on. A composing service
/// implements it over its stock engine's procurement surface (rule selection → move
/// minting → picking projection) and its move-backed delivery reads.
#[async_trait::async_trait]
pub trait StockFulfillmentPort: Send + Sync {
    /// Launch the stock rules for a confirmed order's lines (idempotent per `order_id` —
    /// see the module doc). Called BEFORE the confirm's status flip commits; an `Err`
    /// refuses the whole confirm (the order stays draft) so an order is never confirmed
    /// with its fulfillment silently missing.
    async fn launch_stock_rules(
        &self,
        req: &StockRuleRequest,
    ) -> Result<Vec<StockRuleOutcome>, StockFulfillmentError>;

    /// Read the raw move-backed delivery figures for an order's lines. Lines with no
    /// move-backed figure are omitted from the response (absence, not zero).
    async fn delivered_quantities(
        &self,
        req: &DeliveredQtyRequest,
    ) -> Result<Vec<MoveDeliveryFigures>, StockFulfillmentError>;

    /// Log "decrease ordered quantity" activities on the upstream fulfillment records of
    /// a CANCELLED order. Selling never un-reserves stock through this port —
    /// the activities are the loud channel that tells the stock side a confirmed demand
    /// went away. Idempotent per order in the same spirit as the launch.
    async fn log_decrease_quantity(
        &self,
        req: &DecreaseQuantityRequest,
    ) -> Result<(), StockFulfillmentError>;
}

/// Explicit "no stock engine composed" adapter: every line resolves as not stock-tracked
/// (nothing launches), the reconstruction reads as total absence (no figures — the stored
/// watermarks stand), and the decrease-quantity log is a no-op.
///
/// Hosts opt in deliberately — the guarded route factory REQUIRES a port, so a forgotten
/// stock adapter is a compile error, not silently unfulfilled orders. Composing with this
/// adapter means confirmed orders never launch fulfillment and cancel never logs upstream
/// activities; the composition that has a stock engine supplies the real adapter instead.
pub struct NoStockFulfillmentPort;

#[async_trait::async_trait]
impl StockFulfillmentPort for NoStockFulfillmentPort {
    async fn launch_stock_rules(
        &self,
        _req: &StockRuleRequest,
    ) -> Result<Vec<StockRuleOutcome>, StockFulfillmentError> {
        // Not an error: "no stock engine" means no line is stock-tracked from this
        // composition's point of view. The confirm proceeds without fulfillment.
        Ok(_req
            .lines
            .iter()
            .map(|l| StockRuleOutcome {
                line_id: l.line_id,
                launched: false,
                move_id: None,
                picking_id: None,
                procure_method: None,
            })
            .collect())
    }

    async fn delivered_quantities(
        &self,
        _req: &DeliveredQtyRequest,
    ) -> Result<Vec<MoveDeliveryFigures>, StockFulfillmentError> {
        // Total absence: no move-backed figures exist. The caller keeps every stored
        // watermark untouched — absence is never zero (zero would erase watermarks a
        // previous inbound delivery event legitimately advanced).
        Ok(Vec::new())
    }

    async fn log_decrease_quantity(
        &self,
        _req: &DecreaseQuantityRequest,
    ) -> Result<(), StockFulfillmentError> {
        Ok(())
    }
}
