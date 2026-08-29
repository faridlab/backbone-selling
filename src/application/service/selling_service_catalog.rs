//! Service-tracking catalog port (hand-authored, user-owned) — selling's read of a product's
//! service-delivery policy off the product surface.
//!
//! A product can carry a **service-tracking policy**: when a sales order containing it is
//! confirmed, that policy decides what delivery work is minted for the line —
//!
//! - `task_global_project` — one task per line, under a FIXED project the product names;
//! - `task_in_project` — one project for the whole ORDER (forked from a template the product
//!   names when one is set, else fresh) plus one task per line;
//! - `project_only` — the per-order project, no tasks;
//! - `manual` — nothing is minted; the line's delivery is tracked by hand.
//!
//! The policy and its two anchors (the fixed project, the fork template) live on the product
//! surface — the inventory module's stock-items projection of the catalog item. Selling must
//! not know where or how they are stored, and must not take a Cargo edge to inventory: this
//! file holds only the `ServiceCatalogPort` trait + selling's own DTOs, and a composing
//! service implements the port over its product surface (same posture as
//! [`super::selling_unit_cost`]'s `UnitCostPort` and [`super::selling_stock_fulfillment`]'s
//! `StockFulfillmentPort`).
//!
//! Absence semantics (load-bearing): an item MISSING from the resolution result is the
//! `manual` policy — the product surface holds no tracking row for it, which is exactly what
//! "tracked by hand" means. This deliberately differs from the unit-cost port, where a
//! missing item refuses the confirm: an unknown COST corrupts money analytics, while an
//! untracked product merely mints nothing, which is a legitimate configuration.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One rung of the service-tracking ladder. The wire order mirrors the ladder's scope, from
/// most to least structured; `manual` is the safe default an unconfigured product reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServiceTrackingRung {
    /// One task per order line under the fixed project the product names.
    TaskGlobalProject,
    /// One project per ORDER (template fork when the product names one) plus one task per line.
    TaskInProject,
    /// The per-order project only — delivery is tracked on the project, not on tasks.
    ProjectOnly,
    /// No delivery work is minted; the line is tracked by hand.
    Manual,
}

impl std::fmt::Display for ServiceTrackingRung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ServiceTrackingRung::TaskGlobalProject => "task_global_project",
            ServiceTrackingRung::TaskInProject => "task_in_project",
            ServiceTrackingRung::ProjectOnly => "project_only",
            ServiceTrackingRung::Manual => "manual",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for ServiceTrackingRung {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "task_global_project" => Ok(ServiceTrackingRung::TaskGlobalProject),
            "task_in_project" => Ok(ServiceTrackingRung::TaskInProject),
            "project_only" => Ok(ServiceTrackingRung::ProjectOnly),
            "manual" => Ok(ServiceTrackingRung::Manual),
            other => Err(format!("unknown service-tracking rung '{other}'")),
        }
    }
}

/// One product's service-tracking policy as the confirm path needs it: the rung plus the two
/// anchors the rung's mint keys on. Both anchors are logical references into the project
/// module (Project.id / ProjectTemplate.id) — no cross-module key exists by design.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceTrackingInfo {
    pub item_id: Uuid,
    pub service_tracking: ServiceTrackingRung,
    /// The fixed project for `task_global_project` (the task's home). `None` on the other
    /// rungs — and a `task_global_project` product whose anchor is missing is the mint port's
    /// loud refusal, never a silent skip.
    pub service_project_id: Option<Uuid>,
    /// The blueprint a `task_in_project` / `project_only` order project is forked from.
    /// `None` = fork a fresh empty project instead.
    pub service_project_template_id: Option<Uuid>,
}

/// The product-surface reader's refusal (transport failure, unreadable projection, ...).
/// Flat `{code, message}` by design — the implementing side's own error taxonomy stays on its
/// side of the port (same shape as [`super::selling_unit_cost::UnitCostError`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceCatalogError {
    pub code: String,
    pub message: String,
}

/// The product-policy seam selling's confirm reads before it mints service delivery. A
/// composing service implements it over the product surface's service-tracking columns,
/// company-scoped.
#[async_trait::async_trait]
pub trait ServiceCatalogPort: Send + Sync {
    /// Resolve the service-tracking policy for each named item. Items with no tracking
    /// configuration are OMITTED from the result — omission is the `manual` policy (see the
    /// module doc). Called BEFORE the confirm transaction; an `Err` refuses the whole confirm.
    async fn resolve_service_tracking(
        &self,
        company_id: Uuid,
        item_ids: &[Uuid],
    ) -> Result<Vec<ServiceTrackingInfo>, ServiceCatalogError>;
}

/// Explicit "no product surface composed" adapter: every item resolves as untracked. Combined
/// with [`super::selling_service_delivery::NoServiceDelivery`] this keeps an unwired
/// composition behaving exactly like the era before the seam existed — nothing mints, nothing
/// is stamped. Hosts opt in deliberately; the guarded route factory REQUIRES the port, so a
/// forgotten adapter is a compile error, not silently unconfigured products.
pub struct NoServiceCatalog;

#[async_trait::async_trait]
impl ServiceCatalogPort for NoServiceCatalog {
    async fn resolve_service_tracking(
        &self,
        _company_id: Uuid,
        _item_ids: &[Uuid],
    ) -> Result<Vec<ServiceTrackingInfo>, ServiceCatalogError> {
        // Total absence: no product carries a tracking policy from this composition's point of
        // view, so every line reads as manual and mints nothing. Not an error.
        Ok(Vec::new())
    }
}
