use serde::{Deserialize, Serialize};
use sqlx::Type;
use std::str::FromStr;
#[cfg(feature = "openapi")]
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[cfg_attr(feature = "openapi", derive(ToSchema))]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "sales_invoice_status", rename_all = "snake_case")]
pub enum SalesInvoiceStatus {
    Draft,
    Submitted,
    PartiallyPaid,
    Paid,
    Cancelled,
}

impl std::fmt::Display for SalesInvoiceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::Submitted => write!(f, "submitted"),
            Self::PartiallyPaid => write!(f, "partially_paid"),
            Self::Paid => write!(f, "paid"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl FromStr for SalesInvoiceStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "draft" => Ok(Self::Draft),
            "submitted" => Ok(Self::Submitted),
            "partially_paid" => Ok(Self::PartiallyPaid),
            "paid" => Ok(Self::Paid),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(format!("Unknown SalesInvoiceStatus variant: {}", s)),
        }
    }
}

impl Default for SalesInvoiceStatus {
    fn default() -> Self {
        Self::Draft
    }
}
