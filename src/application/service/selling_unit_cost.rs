//! Inbound unit-cost port (hand-authored, user-owned) — selling's side of the catalog cost seam.
//!
//! Confirming a sales order snapshots each line's cost-per-unit from the catalog's
//! `Item.standard_cost` so margin can be computed against the cost that was current at the
//! moment of commitment (the Odoo `purchase_price` shape). Selling holds only the
//! `UnitCostPort` trait + its own DTOs; a composing service wires a catalog adapter behind it.
//! **Zero normal Cargo edge** to catalog — the DTOs are the wire contract, duplicated per
//! consumer by design (same posture as [`super::selling_cart_pricing`]'s `CartPricingPort`).
//!
//! Batch semantics: the request carries the DISTINCT item ids of an order's lines (not one
//! entry per line), so a duplicate-item order makes ONE port round trip; the confirm flow
//! expands the per-item results back onto each line.
//!
//! `unit_cost: None` in a result means "no cost maintained for this item" — an honest absence.
//! A confirm PROCEEDS with a NULL snapshot for such lines; margin reads NULL for them, never
//! zero (zero is a real zero-margin trade). A port *error*, a missing requested item, or a
//! negative cost REFUSES the confirm (see `confirm_sales_order`).

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The distinct items whose unit costs a confirm needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitCostRequest {
    pub company_id: Uuid,
    pub item_ids: Vec<Uuid>,
}

/// One item's resolved cost. `None` = no cost maintained (honest absence — the line snapshots
/// NULL and margin reads NULL, never zero).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ItemUnitCost {
    pub item_id: Uuid,
    pub unit_cost: Option<Decimal>,
}

/// The composing service's rejection (catalog unavailable, lookup failure).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnitCostError {
    pub code: String,
    pub message: String,
}

/// The unit-cost seam selling's confirm depends on. A composing service implements it over
/// catalog's standard-cost read (`Item.standard_cost`, nullable per company).
#[async_trait::async_trait]
pub trait UnitCostPort: Send + Sync {
    async fn resolve_unit_costs(&self, req: &UnitCostRequest) -> Result<Vec<ItemUnitCost>, UnitCostError>;
}

/// Explicit "no cost source composed" adapter: every requested item resolves to NULL.
///
/// Hosts opt in deliberately — the guarded route factory REQUIRES a port, so a forgotten
/// catalog adapter is a compile error, not silent NULL margins. Composing with this adapter
/// means every confirmed order reads margin NULL (coverage counters on the margin DTO make
/// that visible) — useful for hosts that have not adopted catalog costs yet.
pub struct NoUnitCostPort;

#[async_trait::async_trait]
impl UnitCostPort for NoUnitCostPort {
    async fn resolve_unit_costs(&self, req: &UnitCostRequest) -> Result<Vec<ItemUnitCost>, UnitCostError> {
        Ok(req.item_ids.iter().map(|id| ItemUnitCost { item_id: *id, unit_cost: None }).collect())
    }
}
