use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::str::FromStr;
#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "invoice_policy", rename_all = "snake_case")]
pub enum InvoicePolicy {
    Order,
    Delivery,
}

impl std::fmt::Display for InvoicePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Order => write!(f, "order"),
            Self::Delivery => write!(f, "delivery"),
        }
    }
}

impl FromStr for InvoicePolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "order" => Ok(Self::Order),
            "delivery" => Ok(Self::Delivery),
            _ => Err(format!("Unknown InvoicePolicy variant: {}", s)),
        }
    }
}

impl Default for InvoicePolicy {
    fn default() -> Self {
        Self::Order
    }
}
