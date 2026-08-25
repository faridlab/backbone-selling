use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use rust_decimal::Decimal;

use super::ExpenseReinvoiceState;
use super::AuditMetadata;

/// Strongly-typed ID for ExpenseReinvoiceLink
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExpenseReinvoiceLinkId(pub Uuid);

impl ExpenseReinvoiceLinkId {
    pub fn new(id: Uuid) -> Self { Self(id) }
    pub fn generate() -> Self { Self(Uuid::new_v4()) }
    pub fn into_inner(self) -> Uuid { self.0 }
}

impl std::fmt::Display for ExpenseReinvoiceLinkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for ExpenseReinvoiceLinkId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

impl From<Uuid> for ExpenseReinvoiceLinkId {
    fn from(id: Uuid) -> Self { Self(id) }
}

impl From<ExpenseReinvoiceLinkId> for Uuid {
    fn from(id: ExpenseReinvoiceLinkId) -> Self { id.0 }
}

impl AsRef<Uuid> for ExpenseReinvoiceLinkId {
    fn as_ref(&self) -> &Uuid { &self.0 }
}

impl std::ops::Deref for ExpenseReinvoiceLinkId {
    type Target = Uuid;
    fn deref(&self) -> &Self::Target { &self.0 }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ExpenseReinvoiceLink {
    pub id: Uuid,
    pub company_id: Uuid,
    pub order_id: Uuid,
    pub expense_id: Uuid,
    pub amount: Decimal,
    pub state: ExpenseReinvoiceState,
    #[serde(default)]
    #[sqlx(json)]
    pub metadata: AuditMetadata,
}

impl ExpenseReinvoiceLink {
    /// Create a builder for ExpenseReinvoiceLink
    pub fn builder() -> ExpenseReinvoiceLinkBuilder {
        <ExpenseReinvoiceLinkBuilder as Default>::default()
    }

    /// Create a new ExpenseReinvoiceLink with required fields
    pub fn new(company_id: Uuid, order_id: Uuid, expense_id: Uuid, amount: Decimal, state: ExpenseReinvoiceState) -> Self {
        Self {
            id: Uuid::new_v4(),
            company_id,
            order_id,
            expense_id,
            amount,
            state,
            metadata: AuditMetadata::default(),
        }
    }

    /// Get the entity's unique identifier
    pub fn id(&self) -> &Uuid {
        &self.id
    }

    /// Get a strongly-typed ID for this entity
    pub fn typed_id(&self) -> ExpenseReinvoiceLinkId {
        ExpenseReinvoiceLinkId(self.id)
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
    // Partial Update
    // ==========================================================

    /// Apply partial updates from a map of field name to JSON value
    pub fn apply_patch(&mut self, fields: std::collections::HashMap<String, serde_json::Value>) {
        for (key, value) in fields {
            match key.as_str() {
                "company_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.company_id = v; }
                }
                "order_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.order_id = v; }
                }
                "expense_id" => {
                    if let Ok(v) = serde_json::from_value(value) { self.expense_id = v; }
                }
                "amount" => {
                    if let Ok(v) = serde_json::from_value(value) { self.amount = v; }
                }
                "state" => {
                    if let Ok(v) = serde_json::from_value(value) { self.state = v; }
                }
                _ => {} // ignore unknown fields
            }
        }
    }

    // <<< CUSTOM METHODS START >>>
    // <<< CUSTOM METHODS END >>>
}

impl super::Entity for ExpenseReinvoiceLink {
    type Id = Uuid;

    fn entity_id(&self) -> &Self::Id {
        &self.id
    }

    fn entity_type() -> &'static str {
        "ExpenseReinvoiceLink"
    }
}

impl backbone_core::PersistentEntity for ExpenseReinvoiceLink {
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

impl backbone_orm::EntityRepoMeta for ExpenseReinvoiceLink {
    fn column_types() -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("id".to_string(), "uuid".to_string());
        m.insert("company_id".to_string(), "uuid".to_string());
        m.insert("order_id".to_string(), "uuid".to_string());
        m.insert("expense_id".to_string(), "uuid".to_string());
        m.insert("state".to_string(), "expense_reinvoice_state".to_string());
        m
    }
    fn search_fields() -> &'static [&'static str] {
        &[]
    }
    fn company_field() -> Option<&'static str> {
        Some("company_id")
    }
}

/// Builder for ExpenseReinvoiceLink entity
///
/// Provides a fluent API for constructing ExpenseReinvoiceLink instances.
/// System fields (id, metadata, timestamps) are auto-initialized.
#[derive(Debug, Clone, Default)]
pub struct ExpenseReinvoiceLinkBuilder {
    company_id: Option<Uuid>,
    order_id: Option<Uuid>,
    expense_id: Option<Uuid>,
    amount: Option<Decimal>,
    state: Option<ExpenseReinvoiceState>,
}

impl ExpenseReinvoiceLinkBuilder {
    /// Set the company_id field (required)
    pub fn company_id(mut self, value: Uuid) -> Self {
        self.company_id = Some(value);
        self
    }

    /// Set the order_id field (required)
    pub fn order_id(mut self, value: Uuid) -> Self {
        self.order_id = Some(value);
        self
    }

    /// Set the expense_id field (required)
    pub fn expense_id(mut self, value: Uuid) -> Self {
        self.expense_id = Some(value);
        self
    }

    /// Set the amount field (required)
    pub fn amount(mut self, value: Decimal) -> Self {
        self.amount = Some(value);
        self
    }

    /// Set the state field (default: `ExpenseReinvoiceState::default()`)
    pub fn state(mut self, value: ExpenseReinvoiceState) -> Self {
        self.state = Some(value);
        self
    }

    /// Build the ExpenseReinvoiceLink entity
    ///
    /// Returns Err if any required field without a default is missing.
    pub fn build(self) -> Result<ExpenseReinvoiceLink, String> {
        let company_id = self.company_id.ok_or_else(|| "company_id is required".to_string())?;
        let order_id = self.order_id.ok_or_else(|| "order_id is required".to_string())?;
        let expense_id = self.expense_id.ok_or_else(|| "expense_id is required".to_string())?;
        let amount = self.amount.ok_or_else(|| "amount is required".to_string())?;

        Ok(ExpenseReinvoiceLink {
            id: Uuid::new_v4(),
            company_id,
            order_id,
            expense_id,
            amount,
            state: self.state.unwrap_or_default(),
            metadata: AuditMetadata::default(),
        })
    }
}
