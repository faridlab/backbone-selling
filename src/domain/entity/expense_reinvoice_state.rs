use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::str::FromStr;
#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "expense_reinvoice_state", rename_all = "snake_case")]
pub enum ExpenseReinvoiceState {
    Pending,
    Invoiced,
}

impl std::fmt::Display for ExpenseReinvoiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Invoiced => write!(f, "invoiced"),
        }
    }
}

impl FromStr for ExpenseReinvoiceState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "pending" => Ok(Self::Pending),
            "invoiced" => Ok(Self::Invoiced),
            _ => Err(format!("Unknown ExpenseReinvoiceState variant: {}", s)),
        }
    }
}

impl Default for ExpenseReinvoiceState {
    fn default() -> Self {
        Self::Pending
    }
}
