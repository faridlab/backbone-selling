//! Outbound project-fulfillment port (hand-authored, user-owned) — selling's side of the
//! service-delivery confirm engine.
//!
//! Confirming a sales order that carries a service-tracked product COMMITS delivery work: the
//! project side mints the project/task that work lives in, per the product's service-tracking
//! policy (see [`super::selling_service_catalog`] for the ladder). Selling does not — and must
//! not — know how projects fork from templates or how tasks mint: that is the project
//! module's vocabulary. This file holds only the `ProjectFulfillmentPort` trait + selling's
//! own DTOs; a composing service implements the port over its project write surface. **Zero
//! normal Cargo edge** to the project module — the DTOs are the wire contract, duplicated per
//! consumer by design (same posture as [`super::selling_stock_fulfillment`]'s
//! `StockFulfillmentPort`).
//!
//! Idempotency contract (the load-bearing rule): `mint_service_delivery` MUST be idempotent
//! per sale line — a repeated mint for a line that already minted returns the prior outcome
//! instead of minting again. Implementations key the per-line task on the order-line id (an
//! origin-key unique backstop) and the per-order project on the order id, so a concurrent
//! duplicate confirm or a confirm retried after a lost guard race never double-mints. Selling
//! calls the port BEFORE its confirm transaction commits (the same posture as the stock-rule
//! launch), which is exactly why this contract matters: in the crash window where the mint
//! landed but the confirm transaction failed, the work exists for a still-draft order, and
//! the retried confirm re-mints as a no-op.
//!
//! Selling stamps what the mint reports back onto its own order lines
//! (`sales_order_items.project_id` / `task_id`) INSIDE the confirm transaction — the backref
//! is selling's only record of the mint, and an outcome of `minted: false` (a manual or
//! untracked line) stamps nothing.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::selling_service_catalog::ServiceTrackingRung;

/// One line's delivery commitment — the sale-side expression of "this confirmed line needs a
/// project/task to be delivered in". Carries the line's resolved policy (rung + anchors) so
/// the implementing side decides the mint shape without a second lookup. Downpayment lines
/// never appear here (a downpayment's placeholder quantity is never delivered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDeliveryLine {
    /// The selling order line — the mint's per-line idempotency key and the backref target.
    pub sale_line_id: Uuid,
    pub item_id: Uuid,
    pub quantity: Decimal,
    pub description: Option<String>,
    /// The product's resolved service-tracking rung. `manual` lines are sent too and come
    /// back `minted: false` — a skip, not an error (the port is the decider, mirroring how
    /// the stock port decides storable-ness).
    pub rung: ServiceTrackingRung,
    /// The fixed project for `task_global_project` (from the product's anchor).
    pub fixed_project_id: Option<Uuid>,
    /// The template a per-order project forks from, when the product names one.
    pub template_id: Option<Uuid>,
}

/// The mint request a confirm emits: one confirmed order, one request. The order's identity
/// (id + number) is the per-order project's correspondence key — a `task_in_project` /
/// `project_only` mint finds its prior project through it on a repeat.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDeliveryRequest {
    pub order_id: Uuid,
    pub company_id: Uuid,
    pub customer_id: Uuid,
    /// The order number — the human correspondence key on the minted project.
    pub order_number: String,
    /// The order's currency, so the minted project's financials speak the order's money.
    pub currency: String,
    pub lines: Vec<ServiceDeliveryLine>,
}

/// What the mint did for one line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDeliveryLineOutcome {
    pub sale_line_id: Uuid,
    /// `false` = the line's policy mints nothing (manual rung, or a product the composition
    /// does not track) — not a failure. The confirm proceeds and stamps no backref.
    pub minted: bool,
    /// The project the line's delivery lives in. `None` when `minted` is false.
    pub project_id: Option<Uuid>,
    /// The task minted for the line, when its policy mints tasks. `None` for `project_only`
    /// and unminted lines.
    pub task_id: Option<Uuid>,
}

/// The project side's refusal (transport failure, a `task_global_project` product whose fixed
/// project is missing, ...). Flat `{code, message}` by design — the implementing engine's own
/// error taxonomy stays on its side of the port (same shape as
/// [`super::selling_stock_fulfillment::StockFulfillmentError`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectFulfillmentError {
    pub code: String,
    pub message: String,
}

/// The service-delivery seam selling's confirm depends on. A composing service implements it
/// over its project write surface (per-order project mint keyed on the order, per-line task
/// mint keyed on the sale line).
#[async_trait::async_trait]
pub trait ProjectFulfillmentPort: Send + Sync {
    /// Mint the delivery work for a confirmed order's lines (idempotent per sale line — see
    /// the module doc). Called BEFORE the confirm's status flip commits; an `Err` refuses the
    /// whole confirm (the order stays draft) so an order is never confirmed with its delivery
    /// work silently missing.
    async fn mint_service_delivery(
        &self,
        req: &ServiceDeliveryRequest,
    ) -> Result<Vec<ServiceDeliveryLineOutcome>, ProjectFulfillmentError>;
}

/// Explicit "no project engine composed" adapter: every line comes back unminted — nothing
/// is stamped, the confirm proceeds exactly as it did before the seam existed. Hosts opt in
/// deliberately; the guarded route factory REQUIRES the port, so a forgotten adapter is a
/// compile error, not silently undelivered service orders.
pub struct NoServiceDelivery;

#[async_trait::async_trait]
impl ProjectFulfillmentPort for NoServiceDelivery {
    async fn mint_service_delivery(
        &self,
        req: &ServiceDeliveryRequest,
    ) -> Result<Vec<ServiceDeliveryLineOutcome>, ProjectFulfillmentError> {
        // Not an error: "no project engine" means no line's product is service-tracked from
        // this composition's point of view. The confirm proceeds without delivery minting.
        Ok(req
            .lines
            .iter()
            .map(|l| ServiceDeliveryLineOutcome {
                sale_line_id: l.sale_line_id,
                minted: false,
                project_id: None,
                task_id: None,
            })
            .collect())
    }
}
