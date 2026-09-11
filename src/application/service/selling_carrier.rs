//! The delivery-carrier registry (hand-authored, user-owned) — create/update/list over the
//! carrier master, plus the order's carrier/tracking metadata verb.
//!
//! REGISTRY ONLY (the fence): master data + the order link — no rates, no labels, no carrier
//! API surface, no changes to the `DeliveryRequested` envelope (inventory consumes item/qty
//! only; expanding its contract is out of scope).
//!
//! Retirement path: DEACTIVATE (`active = false`), don't delete. Orders reference a carrier
//! through an FK, so a hard delete of a referenced carrier is blocked by the database; the
//! deactivate flag keeps history readable while retiring the name from the active set.
//!
//! `set_order_delivery` writes fulfillment metadata, NOT frozen money fields: carrier choice
//! and tracking number are writable on draft AND confirmed orders (tracking typically arrives
//! only after ship) and refused only on `cancelled` orders.
//!
//! `expense_id`-style faith applies nowhere here — the carrier is intra-module and verified by
//! a pre-read on every path that names one (create-with-carrier, set-delivery): an unknown
//! carrier id is a clean `CarrierNotFound`, never the FK violation's 500. Under a composed
//! tenancy decorator (ADR-0029) a foreign carrier id is indistinguishable from a missing one —
//! which is the point.
//!
//! An `impl SellingWriteService` chunk over the vocabulary in [`super::selling_write_service`].
//! Per the module's 4-layer rule this file holds no SQL — the statements live on
//! `DeliveryCarrierRepository` / `SalesOrderRepository`.

use uuid::Uuid;

use super::selling_write_service::{
    is_dup, relay_ambient_scope, SellingError, SellingWriteService,
};

/// The carrier-master patch. `None` keeps the stored value. `tracking_url_template` is
/// `Option<Option<String>>`: `None` = not asked (keep), `Some(None)` = explicitly CLEAR the
/// template, `Some(Some(url))` = set it.
#[derive(Debug, Clone, Default)]
pub struct UpdateCarrierPatch {
    pub name: Option<String>,
    pub active: Option<bool>,
    pub tracking_url_template: Option<Option<String>>,
}

impl SellingWriteService {
    /// Create a carrier in the registry. A duplicate live name refuses with `CarrierDuplicate`
    /// (soft-deleted names are reusable): the service pre-reads the name (the module ships no
    /// unique of its own — ADR-0029); under a composed tenancy decorator the decorator's
    /// per-unit unique is the hard backstop.
    pub async fn create_delivery_carrier(
        &self,
        name: &str,
        tracking_url_template: Option<&str>,
    ) -> Result<Uuid, SellingError> {
        let dup = self.repos.carriers.find_carrier_by_name(&self.db_pool, name).await?;
        if dup.is_some() {
            return Err(SellingError::CarrierDuplicate(name.to_string()));
        }
        let id = Uuid::new_v4();
        let r = self.repos.carriers.insert_carrier(&self.db_pool, id, name, tracking_url_template).await;
        if let Err(e) = r {
            return Err(if is_dup(&e) { SellingError::CarrierDuplicate(name.to_string()) } else { e.into() });
        }
        Ok(id)
    }

    /// Update a carrier's master fields (guarded). Unknown id ⇒ `CarrierNotFound`. See
    /// [`UpdateCarrierPatch`] for the patch semantics (including clearing the tracking template).
    pub async fn update_delivery_carrier(
        &self,
        carrier_id: Uuid,
        patch: UpdateCarrierPatch,
    ) -> Result<(), SellingError> {
        if patch.name.is_none() && patch.active.is_none() && patch.tracking_url_template.is_none() {
            return Ok(()); // nothing asked, nothing changed
        }
        let mut tx = self.db_pool.begin().await?;
        relay_ambient_scope(&mut tx).await?;
        let updated = self.repos.carriers.update_carrier(
            &mut tx,
            carrier_id,
            patch.name.as_deref(),
            patch.active,
            patch.tracking_url_template.is_some(),
            patch.tracking_url_template.unwrap_or(None).as_deref(),
        ).await?;
        if !updated {
            // Guarded statement matched nothing: unknown or soft-deleted (under a composed
            // decorator also another tenant's — indistinguishable, which is the point).
            return Err(SellingError::CarrierNotFound(carrier_id));
        }
        tx.commit().await?;
        Ok(())
    }

    /// List the carriers (name order), optionally active-only. Under a composed tenancy
    /// decorator the request's fence scopes the read; an undecorated deployment lists all.
    pub async fn list_delivery_carriers(
        &self,
        active_only: bool,
    ) -> Result<Vec<CarrierDto>, SellingError> {
        let rows = self.repos.carriers.list_carriers(&self.db_pool, active_only).await?;
        Ok(rows.into_iter().map(|c| CarrierDto {
            id: c.id,
            name: c.name,
            active: c.active,
            tracking_url_template: c.tracking_url_template,
        }).collect())
    }

    /// Set an order's delivery metadata: carrier choice + tracking number. Writable on draft
    /// AND confirmed orders; refused only on `cancelled` (`InvalidTransition`). An unknown
    /// carrier id refuses with `CarrierNotFound` (pre-read — never the FK violation's 500). A
    /// wrong-state/absent order id is `InvalidTransition`/`OrderNotFound` (no leak).
    ///
    /// Patch semantics (the `UpdateCarrierPatch` convention): a `None` field KEEPS the stored
    /// value, `Some(None)` CLEARS it, `Some(Some(v))` SETS it — so "add the tracking number that
    /// just arrived" does not have to re-send the carrier.
    pub async fn set_order_delivery(
        &self,
        order_id: Uuid,
        delivery_carrier_id: Option<Option<Uuid>>,
        tracking_ref: Option<Option<String>>,
    ) -> Result<(), SellingError> {
        if let Some(id) = delivery_carrier_id.flatten() {
            self.carrier_id_or_refuse(Some(id)).await?;
        }
        let mut tx = self.db_pool.begin().await?;
        relay_ambient_scope(&mut tx).await?;
        let updated = self.repos.orders.set_order_delivery(
            &mut tx,
            order_id,
            delivery_carrier_id.is_some(),
            delivery_carrier_id.flatten(),
            tracking_ref.is_some(),
            tracking_ref.flatten().as_deref(),
        ).await?;
        if !updated {
            // The guard refused — classify why (only after a refusal; the guarded statement
            // itself never leaks whether the id exists).
            let why = self.repos.orders.find_cancel_refusal(&self.db_pool, order_id).await?;
            return Err(match why {
                None => SellingError::OrderNotFound(order_id),
                Some(r) => SellingError::InvalidTransition { verb: "set_delivery".into(), current: r.status },
            });
        }
        tx.commit().await?;
        Ok(())
    }

    /// Validate a create-time carrier choice: the id must name a LIVE carrier. `None` passes
    /// through (no carrier chosen). Shared by `create_sales_order` and `set_order_delivery`.
    pub(super) async fn carrier_id_or_refuse(
        &self,
        carrier_id: Option<Uuid>,
    ) -> Result<Option<Uuid>, SellingError> {
        match carrier_id {
            None => Ok(None),
            Some(id) => {
                let found = self.repos.carriers.find_carrier(&self.db_pool, id).await?;
                match found {
                    Some(_) => Ok(Some(id)),
                    None => Err(SellingError::CarrierNotFound(id)),
                }
            }
        }
    }
}

/// One carrier as the registry's read surface returns it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CarrierDto {
    pub id: uuid::Uuid,
    pub name: String,
    pub active: bool,
    pub tracking_url_template: Option<String>,
}
