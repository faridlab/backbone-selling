use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use rust_decimal::Decimal;
use super::AuditMetadata;

/// Strongly-typed ID for SalesOrderItem
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SalesOrderItemId(pub Uuid);

impl SalesOrderItemId {
    pub fn new(id: Uuid) -> Self { Self(id) }
    pub fn generate() -> Self { Self(Uuid::new_v4()) }
    pub fn into_inner(self) -> Uuid { self.0 }
}

impl std::fmt::Display for SalesOrderItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for SalesOrderItemId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl From<Uuid> for SalesOrderItemId {
    fn from(id: Uuid) -> Self { Self(id) }
}

impl From<SalesOrderItemId> for Uuid {
    fn from(id: SalesOrderItemId) -> Self { id.0 }
}

impl AsRef<Uuid> for SalesOrderItemId {
    fn as_ref(&self) -> &Uuid { &self.0 }
}

impl std::ops::Deref for SalesOrderItemId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target { &self.0 }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SalesOrderItem {
    pub id: Uuid,
    pub order_id: Uuid,
    pub item_id: Uuid,
    pub description: Option<String>,
    pub quantity: Decimal,
    pub unit_price: Decimal,
    pub line_discount: Decimal,
    pub line_amount: Decimal,
    pub billed_qty: Decimal,
    pub delivered_qty: Decimal,
    #[serde(default)]
    #[sqlx(json)]
    pub metadata: AuditMetadata,
}

impl SalesOrderItem {
    /// Create a builder for SalesOrderItem
    pub fn builder() -> SalesOrderItemBuilder {
        SalesOrderItemBuilder::default()
    }

    /// Create a new SalesOrderItem with required fields
    pub fn new(order_id: Uuid, item_id: Uuid, quantity: Decimal, unit_price: Decimal, line_discount: Decimal, line_amount: Decimal, billed_qty: Decimal, delivered_qty: Decimal) -> Self {
        Self {
            id: Uuid::new_v4(),
            order_id,
            item_id,
            description: None,
            quantity,
            unit_price,
            line_discount,
            line_amount,
            billed_qty,
            delivered_qty,
            metadata: AuditMetadata::default(),
        }
    }

    /// Get the entity's unique identifier
    pub fn id(&self) -> &Uuid {
        &self.id
    }

    /// Get a strongly-typed ID for this entity
    pub fn typed_id(&self) -> SalesOrderItemId {
        SalesOrderItemId(self.id)
    }

    /// Get when this entity was created
    pub fn created_at(&self) -> Option<&DateTime<Utc>> {
        self.metadata.created_at.as_ref()
    }

    /// Get when this entity was last updated
    pub fn updated_at(&self) -> Option<&DateTime<Utc>> {
        self.metadata.updated_at.as_ref()
    }

    /// Check if this entity is soft deleted
    pub fn is_deleted(&self) -> bool {
        self.metadata.deleted_at.is_some()
    }

    /// Check if this entity is active (not deleted)
    pub fn is_active(&self) -> bool {
        self.metadata.deleted_at.is_none()
    }

    /// Get when this entity was deleted
    pub fn deleted_at(&self) -> Option<&DateTime<Utc>> {
        self.metadata.deleted_at.as_ref()
    }

    /// Get who created this entity
    pub fn created_by(&self) -> Option<&Uuid> {
        self.metadata.created_by.as_ref()
    }

    /// Get who last updated this entity
    pub fn updated_by(&self) -> Option<&Uuid> {
        self.metadata.updated_by.as_ref()
    }

    /// Get who deleted this entity
    pub fn deleted_by(&self) -> Option<&Uuid> {
        self.metadata.deleted_by.as_ref()
    }


    // ==========================================================
    // Fluent Setters (with_* for optional fields)
    // ==========================================================

    /// Set the description field (chainable)
    pub fn with_description(mut self, value: String) -> Self {
        self.description = Some(value);
        self
    }

    // ==========================================================
    // Partial Update
    // ==========================================================

    /// Apply partial updates from a map of field name to JSON value
    pub fn apply_patch(&mut self, fields: std::collections::HashMap<String, serde_json::Value>) {
        for (key, value) in fields {
            match key.as_str() {
                "order_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.order_id = v; }
                }
                "item_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.item_id = v; }
                }
                "description" => {
                    if let Ok(v) = serde_json::from_value(value) { self.description = v; }
                }
                "quantity" => {
                    if let Ok(v) = serde_json::from_value(value) { self.quantity = v; }
                }
                "unit_price" => {
                    if let Ok(v) = serde_json::from_value(value) { self.unit_price = v; }
                }
                "line_discount" => {
                    if let Ok(v) = serde_json::from_value(value) { self.line_discount = v; }
                }
                "line_amount" => {
                    if let Ok(v) = serde_json::from_value(value) { self.line_amount = v; }
                }
                "billed_qty" => {
                    if let Ok(v) = serde_json::from_value(value) { self.billed_qty = v; }
                }
                "delivered_qty" => {
                    if let Ok(v) = serde_json::from_value(value) { self.delivered_qty = v; }
                }
                _ => {} // ignore unknown fields
            }
        }
    }

    // <<< CUSTOM METHODS START >>>
    // <<< CUSTOM METHODS END >>>
}

impl super::Entity for SalesOrderItem {
    type Id = Uuid;

    fn entity_id(&self) -> &Self::Id {
        &self.id
    }

    fn entity_type() -> &'static str {
        "SalesOrderItem"
    }
}

impl backbone_core::PersistentEntity for SalesOrderItem {
    fn entity_id(&self) -> String {
        self.id.to_string()
    }
    fn set_entity_id(&mut self, id: String) {
        if let Ok(uuid) = uuid::Uuid::parse_str(&id) {
            self.id = uuid;
        }
    }
    fn created_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata.created_at
    }
    fn set_created_at(&mut self, ts: chrono::DateTime<chrono::Utc>) {
        self.metadata.created_at = Some(ts);
    }
    fn updated_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata.updated_at
    }
    fn set_updated_at(&mut self, ts: chrono::DateTime<chrono::Utc>) {
        self.metadata.updated_at = Some(ts);
    }
    fn deleted_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.metadata.deleted_at
    }
    fn set_deleted_at(&mut self, ts: Option<chrono::DateTime<chrono::Utc>>) {
        self.metadata.deleted_at = ts;
    }
}

impl backbone_orm::EntityRepoMeta for SalesOrderItem {
    fn column_types() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("id".to_string(), "uuid".to_string());
        m.insert("order_id".to_string(), "uuid".to_string());
        m.insert("item_id".to_string(), "uuid".to_string());
        m
    }
    fn search_fields() -> &'static [&'static str] {
        &[]
    }
    fn relations() -> &'static [(&'static str, &'static str, &'static str)] {
        &[("order", "sales_orders", "orderId")]
    }
}

/// Builder for SalesOrderItem entity
///
/// Provides a fluent API for constructing SalesOrderItem instances.
/// System fields (id, metadata, timestamps) are auto-initialized.
#[derive(Debug, Clone, Default)]
pub struct SalesOrderItemBuilder {
    order_id: Option<Uuid>,
    item_id: Option<Uuid>,
    description: Option<String>,
    quantity: Option<Decimal>,
    unit_price: Option<Decimal>,
    line_discount: Option<Decimal>,
    line_amount: Option<Decimal>,
    billed_qty: Option<Decimal>,
    delivered_qty: Option<Decimal>,
}

impl SalesOrderItemBuilder {
    /// Set the order_id field (required)
    pub fn order_id(mut self, value: Uuid) -> Self {
        self.order_id = Some(value);
        self
    }

    /// Set the item_id field (required)
    pub fn item_id(mut self, value: Uuid) -> Self {
        self.item_id = Some(value);
        self
    }

    /// Set the description field (optional)
    pub fn description(mut self, value: String) -> Self {
        self.description = Some(value);
        self
    }

    /// Set the quantity field (required)
    pub fn quantity(mut self, value: Decimal) -> Self {
        self.quantity = Some(value);
        self
    }

    /// Set the unit_price field (required)
    pub fn unit_price(mut self, value: Decimal) -> Self {
        self.unit_price = Some(value);
        self
    }

    /// Set the line_discount field (default: `Decimal::from(0)`)
    pub fn line_discount(mut self, value: Decimal) -> Self {
        self.line_discount = Some(value);
        self
    }

    /// Set the line_amount field (default: `Decimal::from(0)`)
    pub fn line_amount(mut self, value: Decimal) -> Self {
        self.line_amount = Some(value);
        self
    }

    /// Set the billed_qty field (default: `Decimal::from(0)`)
    pub fn billed_qty(mut self, value: Decimal) -> Self {
        self.billed_qty = Some(value);
        self
    }

    /// Set the delivered_qty field (default: `Decimal::from(0)`)
    pub fn delivered_qty(mut self, value: Decimal) -> Self {
        self.delivered_qty = Some(value);
        self
    }

    /// Build the SalesOrderItem entity
    ///
    /// Returns Err if any required field without a default is missing.
    pub fn build(self) -> Result<SalesOrderItem, String> {
        let order_id = self.order_id.ok_or_else(|| "order_id is required".to_string())?;
        let item_id = self.item_id.ok_or_else(|| "item_id is required".to_string())?;
        let quantity = self.quantity.ok_or_else(|| "quantity is required".to_string())?;
        let unit_price = self.unit_price.ok_or_else(|| "unit_price is required".to_string())?;

        Ok(SalesOrderItem {
            id: Uuid::new_v4(),
            order_id,
            item_id,
            description: self.description,
            quantity,
            unit_price,
            line_discount: self.line_discount.unwrap_or(Decimal::from(0)),
            line_amount: self.line_amount.unwrap_or(Decimal::from(0)),
            billed_qty: self.billed_qty.unwrap_or(Decimal::from(0)),
            delivered_qty: self.delivered_qty.unwrap_or(Decimal::from(0)),
            metadata: AuditMetadata::default(),
        })
    }
}
